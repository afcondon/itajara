//! **What an aggregate device is actually made of, asked of CoreAudio.**
//!
//! A Mac aggregate presents its members' channels end to end, so a stereo pair
//! on the Audio4c might be channels 17 and 18 today. The order is a property of
//! the aggregate as it currently stands — **not of its name, and not stable
//! across a power cycle or a re-plug.** An interface that was not there when
//! the aggregate was built lands wherever it lands.
//!
//! That makes an absolute channel number the wrong thing to write down. A rig
//! configured as `board=17,18` records the pedalboard until the day the
//! aggregate comes back in the other order, and then it records ES-9 inputs 1
//! and 2 while looking entirely normal: the meters move, the takes have audio
//! in them, and nothing anywhere says the wrong thing was captured.
//!
//! So this asks the system what the aggregate holds, in order, every time the
//! daemon starts — and a source names **a device and a channel on it**, which
//! is resolved against the answer. If the device it names is not in the
//! aggregate, that is a refusal rather than a guess.
//!
//! # It does not open anything
//!
//! Every call here reads a property. Nothing starts a stream, claims a device
//! or changes a sample rate, which matters on a machine where a DAW is running:
//! interfaces coming and going is precisely what upsets one, and a tool that
//! reports the layout must not be a reason for the layout to change.

#![allow(non_upper_case_globals)]

use std::ffi::c_void;

use coreaudio_sys::{
    kAudioAggregateDevicePropertyFullSubDeviceList, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreamConfiguration, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, AudioBufferList, AudioObjectID,
    AudioObjectGetPropertyData, AudioObjectPropertyAddress,
};
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::CFRelease;
use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringGetCString, CFStringGetLength, CFStringRef,
};

/// One member of an aggregate, and where its channels landed.
#[derive(Debug, Clone)]
pub struct Member {
    pub uid: String,
    pub name: String,
    /// Input channels this member contributes.
    pub in_ch: u32,
    /// The aggregate channel its first input is, one-based — what a `--source`
    /// has to say to reach it *today*.
    pub first_in: u32,
}

/// A device, and what it is made of if it is an aggregate.
#[derive(Debug, Clone)]
pub struct Layout {
    pub name: String,
    pub uid: String,
    pub in_ch: u32,
    /// Empty for a plain device. In aggregate order, which is the order the
    /// channels appear in.
    pub members: Vec<Member>,
}

impl Layout {
    pub fn is_aggregate(&self) -> bool {
        !self.members.is_empty()
    }

    /// The member whose name contains this, case-insensitively — the same
    /// matching rule `devices::find` uses, so one spelling works everywhere.
    ///
    /// Ambiguity is an error rather than a first match: on a machine with
    /// aggregates, two members matching one word is exactly the situation
    /// where guessing records the wrong input.
    pub fn member(&self, needle: &str) -> Result<&Member, String> {
        let lower = needle.to_lowercase();
        if let Some(m) = self.members.iter().find(|m| m.name.to_lowercase() == lower) {
            return Ok(m);
        }
        let hits: Vec<&Member> = self
            .members
            .iter()
            .filter(|m| m.name.to_lowercase().contains(&lower))
            .collect();
        match hits.len() {
            1 => Ok(hits[0]),
            0 => Err(format!(
                "{:?} is not in {}. It holds: {}",
                needle,
                self.name,
                self.members
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            _ => Err(format!(
                "{:?} matches more than one member of {}: {}",
                needle,
                self.name,
                hits.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// How it reads on a page: the channel ranges, in order.
    pub fn describe(&self) -> String {
        if !self.is_aggregate() {
            return format!("{} — {} in, not an aggregate", self.name, self.in_ch);
        }
        let mut s = format!("{} — {} in, made of:\n", self.name, self.in_ch);
        for m in &self.members {
            if m.in_ch == 0 {
                s.push_str(&format!("  (no inputs)     {}\n", m.name));
            } else if m.in_ch == 1 {
                s.push_str(&format!("  {:>3}            {}\n", m.first_in, m.name));
            } else {
                s.push_str(&format!(
                    "  {:>3}-{:<3}        {}\n",
                    m.first_in,
                    m.first_in + m.in_ch - 1,
                    m.name
                ));
            }
        }
        s
    }
}

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// A CFString property, as a Rust `String`.
///
/// The caller owns what CoreAudio hands back for a CFString property, so this
/// releases it. Getting that wrong leaks once per device per start, which is
/// small and still wrong.
fn string_prop(id: AudioObjectID, selector: u32) -> Option<String> {
    let a = addr(selector, kAudioObjectPropertyScopeGlobal);
    let mut cf: CFStringRef = std::ptr::null();
    let mut size = std::mem::size_of::<CFStringRef>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut cf as *mut CFStringRef as *mut c_void,
        )
    };
    if st != 0 || cf.is_null() {
        return None;
    }
    let out = cf_string(cf);
    unsafe { CFRelease(cf as *const c_void) };
    out
}

fn cf_string(cf: CFStringRef) -> Option<String> {
    unsafe {
        // Worst case for UTF-8 is four bytes a UTF-16 unit, plus the NUL.
        let cap = (CFStringGetLength(cf) as usize) * 4 + 1;
        let mut buf = vec![0i8; cap.max(2)];
        if CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, kCFStringEncodingUTF8) == 0 {
            return None;
        }
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8(bytes).ok()
    }
}

/// Input channels a device offers, summed over its streams.
///
/// `kAudioDevicePropertyStreamConfiguration` returns a variable-length
/// `AudioBufferList`, so the size is asked for first and the buffer is raw
/// bytes rather than the struct — the struct's single-element array is a C
/// idiom that Rust will not size correctly on its own.
fn input_channels(id: AudioObjectID) -> u32 {
    let a = addr(
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeInput,
    );
    let mut size: u32 = 0;
    let st = unsafe {
        coreaudio_sys::AudioObjectGetPropertyDataSize(id, &a, 0, std::ptr::null(), &mut size)
    };
    if st != 0 || size == 0 {
        return 0;
    }
    let mut bytes = vec![0u8; size as usize];
    let st = unsafe {
        AudioObjectGetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            bytes.as_mut_ptr() as *mut c_void,
        )
    };
    if st != 0 {
        return 0;
    }
    unsafe {
        let list = bytes.as_ptr() as *const AudioBufferList;
        let n = (*list).mNumberBuffers as usize;
        let first = (*list).mBuffers.as_ptr();
        (0..n).map(|i| (*first.add(i)).mNumberChannels).sum()
    }
}

/// Every audio device the system currently has.
fn device_ids() -> Vec<AudioObjectID> {
    let a = addr(kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal);
    let mut size: u32 = 0;
    let st = unsafe {
        coreaudio_sys::AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &a,
            0,
            std::ptr::null(),
            &mut size,
        )
    };
    if st != 0 || size == 0 {
        return Vec::new();
    }
    let n = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut ids = vec![0 as AudioObjectID; n];
    let st = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut c_void,
        )
    };
    if st != 0 {
        return Vec::new();
    }
    ids
}

/// The sub-device UIDs of an aggregate, in the order its channels appear.
/// Empty for anything that is not an aggregate — the property simply is not
/// there, which is how we tell.
fn sub_device_uids(id: AudioObjectID) -> Vec<String> {
    let a = addr(
        kAudioAggregateDevicePropertyFullSubDeviceList,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut cf: CFArrayRef = std::ptr::null();
    let mut size = std::mem::size_of::<CFArrayRef>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            id,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut cf as *mut CFArrayRef as *mut c_void,
        )
    };
    if st != 0 || cf.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        for i in 0..CFArrayGetCount(cf) {
            let s = CFArrayGetValueAtIndex(cf, i) as CFStringRef;
            if !s.is_null() {
                if let Some(u) = cf_string(s) {
                    out.push(u);
                }
            }
        }
        CFRelease(cf as *const c_void);
    }
    out
}

/// Every device, with its aggregate composition worked out.
pub fn layouts() -> Vec<Layout> {
    let ids = device_ids();
    // UID → (name, input channels), so a member can be named and sized without
    // asking the system again for each.
    let by_uid: Vec<(String, String, u32, AudioObjectID)> = ids
        .iter()
        .filter_map(|&id| {
            let uid = string_prop(id, kAudioDevicePropertyDeviceUID)?;
            let name = string_prop(id, kAudioObjectPropertyName)?;
            Some((uid, name, input_channels(id), id))
        })
        .collect();

    by_uid
        .iter()
        .map(|(uid, name, in_ch, id)| {
            let mut members = Vec::new();
            let mut next = 1u32;
            for sub in sub_device_uids(*id) {
                let (mname, mch) = by_uid
                    .iter()
                    .find(|(u, _, _, _)| *u == sub)
                    // A member the system lists but will not describe is a
                    // member that is not plugged in. Named, sized zero, and
                    // kept in place, because dropping it would silently shift
                    // everything after it.
                    .map(|(_, n, c, _)| (n.clone(), *c))
                    .unwrap_or_else(|| (format!("{sub} (absent)"), 0));
                members.push(Member {
                    uid: sub,
                    name: mname,
                    in_ch: mch,
                    first_in: next,
                });
                next += mch;
            }
            Layout {
                name: name.clone(),
                uid: uid.clone(),
                in_ch: *in_ch,
                members,
            }
        })
        .collect()
}

/// The layout of one device, by the same case-insensitive substring rule the
/// rest of the daemon matches devices with.
pub fn layout_of(needle: &str) -> Option<Layout> {
    let lower = needle.to_lowercase();
    let all = layouts();
    all.iter()
        .find(|l| l.name.to_lowercase() == lower)
        .or_else(|| all.iter().find(|l| l.name.to_lowercase().contains(&lower)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, in_ch: u32, first_in: u32) -> Member {
        Member { uid: format!("uid-{name}"), name: name.into(), in_ch, first_in }
    }

    /// The real one, as it stood on 2026-09-08. Third member contributes no
    /// inputs, which is why 16 + 8 + 2 out is 26 while in is 24 — and why the
    /// channel count cannot be inferred from the member count.
    fn es9_then_a4c() -> Layout {
        Layout {
            name: "ES9 then A4C".into(),
            uid: "agg".into(),
            in_ch: 24,
            members: vec![
                member("ES-9", 16, 1),
                member("AUDIO4c", 8, 17),
                member("MacBook Pro Speakers", 0, 25),
            ],
        }
    }

    #[test]
    fn a_member_is_found_by_part_of_its_name() {
        let l = es9_then_a4c();
        assert_eq!(l.member("audio4c").unwrap().first_in, 17);
        assert_eq!(l.member("ES-9").unwrap().first_in, 1);
    }

    /// The whole point: the same spelling gives a different channel when the
    /// aggregate comes back in the other order, instead of quietly giving the
    /// same number and the wrong input.
    #[test]
    fn the_same_source_follows_its_interface_when_the_order_changes() {
        let forward = es9_then_a4c();
        let reversed = Layout {
            name: "A4C then ES9".into(),
            uid: "agg".into(),
            in_ch: 24,
            members: vec![member("AUDIO4c", 8, 1), member("ES-9", 16, 9)],
        };
        assert_eq!(forward.member("AUDIO4c").unwrap().first_in, 17);
        assert_eq!(reversed.member("AUDIO4c").unwrap().first_in, 1);
    }

    #[test]
    fn an_absent_interface_is_named_not_guessed_at() {
        let e = es9_then_a4c().member("Scarlett").unwrap_err();
        assert!(e.contains("not in"), "{e}");
        assert!(e.contains("ES-9"), "it should say what IS there: {e}");
    }

    /// Two members matching one word is exactly when guessing records the
    /// wrong input, so it is an error rather than a first match.
    #[test]
    fn an_ambiguous_name_is_refused() {
        let l = Layout {
            name: "two".into(),
            uid: "agg".into(),
            in_ch: 4,
            members: vec![member("ES-9 A", 2, 1), member("ES-9 B", 2, 3)],
        };
        assert_eq!(l.member("ES-9 B").unwrap().first_in, 3, "an exact name wins outright");
        let e = l.member("ES-9").unwrap_err();
        assert!(e.contains("more than one"), "{e}");
    }

    #[test]
    fn a_plain_device_has_no_members_and_says_so() {
        let l = Layout { name: "AUDIO4c".into(), uid: "u".into(), in_ch: 8, members: vec![] };
        assert!(!l.is_aggregate());
        assert!(l.describe().contains("not an aggregate"));
    }
}
