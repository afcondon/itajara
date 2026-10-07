//! **What the daemon's memory is doing, and what the device list did just before.**
//!
//! On 2026-10-07 the daemon had been up 12½ days with a physical footprint of
//! 11.2 GB, 7 GB of it swapped and so invisible to RSS. The arena was not it.
//! 9.6 GB sat in CoreAudio's `caulk::audio_buffer_resource` malloc zone —
//! ~480k blocks of ~20 KB that this code never allocates itself — and the
//! count stood still at idle, so it grows in bursts. Unplugging the Audio4c
//! from the "ES9 then A4C" aggregate looked like one such burst, **without
//! `supervise` reacting at all**: an aggregate that loses a member keeps
//! running on what remains. The zone also gave blocks back (−11k in an hour),
//! so it is not simply a leak per event.
//!
//! Finding the trigger from outside meant `vmmap`, which suspends the process
//! for ~2 s — the stall threshold — or a forked corpse at ~4.5 s a look. From
//! inside it is a walk of the zone table. So the daemon now reports it itself:
//!
//!   - **at start**, a baseline;
//!   - **whenever CoreAudio's device list changes**, which names arrived and
//!     left, then the memory at once, at +5 s and at +30 s, since a burst may
//!     land after the notification;
//!   - **every ten minutes**, but only if something moved, so a quiet week is a
//!     quiet log.
//!
//! # Not on the audio thread, and not often
//!
//! `malloc_zone_statistics` takes each zone's lock while it counts. The caulk
//! zone is CoreAudio's realtime buffer pool, so a long count could hold up an
//! IO thread that wants it. It runs only on the supervisor's thread, only at
//! the moments above, and each `mem:` line says how long the walk took — if
//! that ever reads more than a millisecond or two, this is the first suspect
//! for a glitch.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use coreaudio_sys::{
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectID,
    AudioObjectPropertyAddress, OSStatus,
};

/// Zones smaller than this are left out of the line unless they moved.
const SHOWN: usize = 64 << 20;
/// After a device change, look again at these offsets.
const FOLLOW_UPS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(30)];
const PERIOD: Duration = Duration::from_secs(600);

/// Bumped by CoreAudio's notification thread; read by the supervisor.
static DEVICES_CHANGED: AtomicU32 = AtomicU32::new(0);

/// One zone, as `malloc_zone_statistics` counts it.
#[derive(Clone)]
struct Zone {
    name: String,
    blocks: u32,
    bytes: usize,
}

#[derive(Clone)]
struct Mem {
    footprint: u64,
    zones: Vec<Zone>,
    took: Duration,
}

extern "C" {
    fn malloc_get_all_zones(
        task: libc::mach_port_t,
        reader: *const libc::c_void,
        addresses: *mut *mut libc::vm_address_t,
        count: *mut libc::c_uint,
    ) -> libc::kern_return_t;
    fn malloc_get_zone_name(zone: *mut libc::malloc_zone_t) -> *const libc::c_char;
}

fn footprint() -> u64 {
    let mut ri: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let st = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            &mut ri as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
        )
    };
    if st == 0 { ri.ri_phys_footprint } else { 0 }
}

fn zones() -> Vec<Zone> {
    let mut addrs: *mut libc::vm_address_t = std::ptr::null_mut();
    let mut n: libc::c_uint = 0;
    // `mach_task_self` is deprecated in libc in favour of the mach2 crate; one
    // call is not worth a dependency.
    #[allow(deprecated)]
    let kr = unsafe {
        malloc_get_all_zones(libc::mach_task_self(), std::ptr::null(), &mut addrs, &mut n)
    };
    if kr != 0 || addrs.is_null() {
        return Vec::new();
    }
    (0..n as usize)
        .map(|i| {
            let z = unsafe { *addrs.add(i) } as *mut libc::malloc_zone_t;
            let mut s: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
            unsafe { libc::malloc_zone_statistics(z, &mut s) };
            let p = unsafe { malloc_get_zone_name(z) };
            let name = if p.is_null() {
                format!("zone {i}")
            } else {
                unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
            };
            Zone { name, blocks: s.blocks_in_use, bytes: s.size_in_use }
        })
        .collect()
}

fn measure() -> Mem {
    let t = Instant::now();
    let zones = zones();
    Mem { footprint: footprint(), zones, took: t.elapsed() }
}

fn mb(b: i128) -> String {
    format!("{:.0} MB", b as f64 / (1 << 20) as f64)
}

fn signed_mb(b: i128) -> String {
    format!("{}{}", if b >= 0 { "+" } else { "" }, mb(b))
}

/// `mem: footprint 11183 MB (+206 MB) · caulk::audio_buffer_resource 468617 blocks (+1203) 9421 MB (+24 MB) · …`
fn line(m: &Mem, prev: Option<&Mem>) -> String {
    let mut s = format!("mem: footprint {}", mb(m.footprint as i128));
    if let Some(p) = prev {
        s += &format!(" ({})", signed_mb(m.footprint as i128 - p.footprint as i128));
    }
    for z in &m.zones {
        let before = prev.and_then(|p| p.zones.iter().find(|q| q.name == z.name));
        let moved = before.is_some_and(|b| b.blocks != z.blocks);
        if z.bytes < SHOWN && !moved {
            continue;
        }
        s += &format!(" · {} {} blocks", z.name, z.blocks);
        if let Some(b) = before {
            s += &format!(" ({:+})", z.blocks as i64 - b.blocks as i64);
        }
        s += &format!(" {}", mb(z.bytes as i128));
        if let Some(b) = before {
            s += &format!(" ({})", signed_mb(z.bytes as i128 - b.bytes as i128));
        }
    }
    s + &format!(" · counted in {:.1} ms", m.took.as_secs_f64() * 1e3)
}

fn moved(a: &Mem, b: &Mem) -> bool {
    a.zones.len() != b.zones.len() || a.zones.iter().zip(&b.zones).any(|(x, y)| x.blocks != y.blocks)
}

unsafe extern "C" fn on_devices(
    _id: AudioObjectID,
    _n: u32,
    _addrs: *const AudioObjectPropertyAddress,
    _data: *mut libc::c_void,
) -> OSStatus {
    DEVICES_CHANGED.fetch_add(1, Ordering::Release);
    0
}

/// The supervisor's view: call `tick` from its loop, as often as it likes.
pub struct Watch {
    names: Vec<String>,
    seen: u32,
    /// The last measurement printed, which the next is compared against.
    last: Mem,
    last_at: Instant,
    /// The device change being followed up, and which follow-ups remain.
    since: Option<(Instant, Mem, usize)>,
}

impl Watch {
    pub fn start() -> Watch {
        let a = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let st = unsafe {
            coreaudio_sys::AudioObjectAddPropertyListener(
                kAudioObjectSystemObject,
                &a,
                Some(on_devices),
                std::ptr::null_mut(),
            )
        };
        if st != 0 {
            eprintln!("mem: could not listen for device changes (OSStatus {st}); periodic lines only");
        }
        let m = measure();
        eprintln!("{}", line(&m, None));
        Watch {
            names: crate::aggregate::device_names(),
            seen: DEVICES_CHANGED.load(Ordering::Acquire),
            last: m,
            last_at: Instant::now(),
            since: None,
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        let c = DEVICES_CHANGED.load(Ordering::Acquire);
        if c != self.seen {
            self.seen = c;
            let names = crate::aggregate::device_names();
            let gone: Vec<_> = self.names.iter().filter(|n| !names.contains(n)).cloned().collect();
            let came: Vec<_> = names.iter().filter(|n| !self.names.contains(n)).cloned().collect();
            self.names = names;
            eprintln!("devices changed: left {:?}, arrived {:?}", gone, came);
            let m = measure();
            eprintln!("{}", line(&m, Some(&self.last)));
            // A second change inside the follow-up window restarts it, measured
            // from the memory as it stood before the first: overlapping changes
            // are one event as far as the cost goes.
            let base = self.since.take().map(|(_, b, _)| b).unwrap_or_else(|| self.last.clone());
            self.since = Some((now, base, 0));
            self.last = m;
            self.last_at = now;
            return;
        }
        if let Some((at, base, next)) = self.since.take() {
            if now - at >= FOLLOW_UPS[next] {
                let m = measure();
                eprintln!(
                    "{} · {} s after the change, against before it",
                    line(&m, Some(&base)),
                    FOLLOW_UPS[next].as_secs()
                );
                self.last = m;
                self.last_at = now;
                if next + 1 < FOLLOW_UPS.len() {
                    self.since = Some((at, base, next + 1));
                }
            } else {
                self.since = Some((at, base, next));
            }
            return;
        }
        if now - self.last_at >= PERIOD {
            let m = measure();
            if moved(&m, &self.last) {
                eprintln!("{}", line(&m, Some(&self.last)));
                self.last = m;
            }
            self.last_at = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walk finds the zones vmmap finds, and sees an allocation land.
    #[test]
    fn counts_what_is_allocated() {
        let before = measure();
        assert!(before.footprint > 0);
        assert!(before.zones.iter().any(|z| z.name.contains("Malloc")), "no default zone among {:?}",
            before.zones.iter().map(|z| &z.name).collect::<Vec<_>>());
        let held: Vec<Box<[u8; 4096]>> = (0..1000).map(|_| Box::new([1u8; 4096])).collect();
        let after = measure();
        let total = |m: &Mem| m.zones.iter().map(|z| z.blocks as i64).sum::<i64>();
        assert!(total(&after) - total(&before) >= 1000);
        assert!(line(&after, Some(&before)).starts_with("mem: footprint "));
        drop(held);
    }
}
