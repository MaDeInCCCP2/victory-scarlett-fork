//! Split a large contiguous shadow read between the caller and one kept
//! worker (`VICTORY_REFRESH_SPLIT_COPY=1`, ported from Kelvaroq `0053-submit.patch`).
//! The worker runs only byte copies and never takes another shadow lock.

use std::sync::{Mutex, OnceLock, mpsc};

fn split_at(from: usize, len: usize) -> Option<usize> {
    const MIN: usize = 2 << 20;
    if len < MIN {
        return None;
    }
    let middle = from.checked_add(len / 2)?;
    Some(middle - middle % crate::watch::PAGE - from)
}

struct Lane {
    send: mpsc::SyncSender<(usize, usize, usize)>,
    done: Mutex<mpsc::Receiver<()>>,
}

impl Lane {
    fn start() -> Option<Self> {
        let (send, work) = mpsc::sync_channel::<(usize, usize, usize)>(1);
        let (signal, done) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("victory-refresh-copy".into())
            .spawn(move || {
                for (dst, from, len) in work {
                    crate::host::copy_mapped(dst as *mut u8, from as *const u8, len, false);
                    if signal.send(()).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            send,
            done: Mutex::new(done),
        })
    }

    fn copy(&self, dst: *mut u8, from: usize, len: usize, first: usize) -> bool {
        let Ok(done) = self.done.try_lock() else {
            return false;
        };
        let tail = (dst.wrapping_add(first) as usize, from + first, len - first);
        if self.send.try_send(tail).is_err() {
            return false;
        }
        struct Finish<'a>(Option<&'a mpsc::Receiver<()>>);
        impl Drop for Finish<'_> {
            fn drop(&mut self) {
                if let Some(done) = self.0 {
                    let _ = done.recv();
                }
            }
        }
        let mut finish = Finish(Some(&done));
        crate::host::copy_mapped(dst, from as *const u8, first, false);
        let ok = done.recv().is_ok();
        finish.0 = None;
        if !ok {
            crate::host::copy_mapped(tail.0 as *mut u8, tail.1 as *const u8, tail.2, false);
        }
        true
    }
}

pub(super) fn copy(dst: *mut u8, from: usize, len: usize) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    static LANE: OnceLock<Option<Lane>> = OnceLock::new();
    let Some(first) = split_at(from, len) else {
        return false;
    };
    if !*ON.get_or_init(|| {
        std::env::var_os("VICTORY_REFRESH_SPLIT_COPY").is_some_and(|v| v == "1")
    }) {
        return false;
    }
    LANE.get_or_init(Lane::start)
        .as_ref()
        .is_some_and(|lane| lane.copy(dst, from, len, first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_copy_covers_unaligned_shadow_and_reuses_lane() {
        let lane = Lane::start().unwrap();
        let src: Vec<u8> = (0..(3 << 20))
            .map(|i: usize| (i.wrapping_mul(19) >> 5) as u8)
            .collect();
        let mut dst = vec![0xa5; src.len()];
        for start in [0, 173, 65535] {
            let len = src.len() - start - 13;
            let from = src.as_ptr() as usize + start;
            let first = split_at(from, len).unwrap();
            assert_eq!((from + first) % crate::watch::PAGE, 0);
            dst.fill(0xa5);
            assert!(lane.copy(dst.as_mut_ptr().wrapping_add(start), from, len, first));
            assert_eq!(dst[start..start + len], src[start..start + len]);
            assert!(
                dst[..start]
                    .iter()
                    .chain(&dst[start + len..])
                    .all(|&x| x == 0xa5)
            );
        }
        assert!(split_at(123, 65536).is_none());
    }
}
