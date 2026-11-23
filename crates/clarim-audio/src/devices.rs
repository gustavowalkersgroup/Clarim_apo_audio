//! WASAPI endpoint enumeration (mapped to `clarim_core::DeviceInfo`), deny-list, virtual-cable detection.

use anyhow::{Context, Result};
use clarim_core::{is_virtual_device, DeviceInfo};
use wasapi::{DeviceCollection, Direction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Capture,
    Render,
}

impl Flow {
    fn direction(self) -> Direction {
        match self {
            Flow::Capture => Direction::Capture,
            Flow::Render => Direction::Render,
        }
    }
}

/// Case-insensitive substring deny-list match (empty patterns match nothing).
pub fn is_denied(name: &str, deny: &[String]) -> bool {
    let lower = name.to_lowercase();
    deny.iter().any(|p| {
        let p = p.trim();
        !p.is_empty() && lower.contains(&p.to_lowercase())
    })
}

/// Enumerate active endpoints for one flow direction, minus denied ones.
/// Must be called from a thread where COM (MTA) is initialized (`crate::init_com`).
pub fn enumerate(flow: Flow, deny: &[String]) -> Result<Vec<DeviceInfo>> {
    let dir = flow.direction();
    let default_id = wasapi::get_default_device(&dir)
        .ok()
        .and_then(|d| d.get_id().ok());

    let collection =
        DeviceCollection::new(&dir).map_err(anyhow_err).context("device collection")?;
    let count = collection.get_nbr_devices().map_err(anyhow_err)?;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let Ok(dev) = collection.get_device_at_index(i) else {
            continue;
        };
        let Ok(id) = dev.get_id() else {
            continue;
        };
        let name = dev
            .get_friendlyname()
            .unwrap_or_else(|_| format!("Device {i}"));
        if is_denied(&name, deny) {
            continue;
        }
        out.push(DeviceInfo {
            is_default: Some(&id) == default_id.as_ref(),
            is_virtual: is_virtual_device(&name),
            id,
            name,
        });
    }
    Ok(out)
}

fn anyhow_err(e: Box<dyn std::error::Error>) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Pick the playback endpoint that feeds the virtual microphone, in this order:
/// VB-Cable "CABLE Input", any "Virtual Audio Driver" / "Virtual Mic" endpoint,
/// SteelSeries Sonar's virtual microphone. Apps then use the cable's capture side as their mic.
pub fn find_virtual_mic(outputs: &[DeviceInfo]) -> Option<&DeviceInfo> {
    const PREFERENCE: [&[&str]; 3] = [
        &["cable input"],
        &["virtual audio driver", "virtual mic"],
        &["sonar - microphone", "sonar microphone"],
    ];
    PREFERENCE.iter().find_map(|group| {
        outputs.iter().find(|d| {
            let n = d.name.to_lowercase();
            group.iter().any(|p| n.contains(p))
        })
    })
}

/// A real microphone: the default one unless it is virtual, otherwise the first non-virtual one.
/// Capturing a virtual cable while rendering into one would create a feedback loop.
pub fn pick_real_mic(inputs: &[DeviceInfo]) -> Option<&DeviceInfo> {
    inputs
        .iter()
        .find(|d| d.is_default && !d.is_virtual)
        .or_else(|| inputs.iter().find(|d| !d.is_virtual))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id: format!("id-{name}"),
            name: name.to_string(),
            is_default,
            is_virtual: is_virtual_device(name),
        }
    }

    #[test]
    fn deny_list_matches_case_insensitive() {
        let deny = vec!["nvidia".to_string(), "LG ULTRAGEAR".to_string()];
        assert!(is_denied("NVIDIA High Definition Audio", &deny));
        assert!(is_denied("2 - LG UltraGear (NVIDIA High Definition Audio)", &deny));
        assert!(!is_denied("Headset Microphone (USB Audio)", &deny));
    }

    #[test]
    fn empty_patterns_deny_nothing() {
        let deny = vec![String::new(), "  ".to_string()];
        assert!(!is_denied("Speakers (Realtek)", &deny));
    }

    #[test]
    fn virtual_mic_preference_order() {
        let outs = [
            dev("Speakers (Realtek(R) Audio)", true),
            dev("SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)", false),
            dev("Virtual Audio Driver Output", false),
            dev("CABLE Input (VB-Audio Virtual Cable)", false),
        ];
        assert_eq!(find_virtual_mic(&outs).map(|d| d.name.as_str()), Some("CABLE Input (VB-Audio Virtual Cable)"));
        assert_eq!(
            find_virtual_mic(&outs[..3]).map(|d| d.name.as_str()),
            Some("Virtual Audio Driver Output")
        );
        assert!(find_virtual_mic(&outs[..2]).is_some_and(|d| d.name.contains("Sonar - Microphone")));
        assert!(find_virtual_mic(&outs[..1]).is_none());
        assert!(find_virtual_mic(&[]).is_none());
    }

    #[test]
    fn real_mic_prefers_default_unless_virtual() {
        let ins = [
            dev("CABLE Output (VB-Audio Virtual Cable)", true),
            dev("Headset Microphone (USB Audio)", false),
            dev("Microphone (Realtek(R) Audio)", false),
        ];
        assert_eq!(pick_real_mic(&ins).map(|d| d.name.as_str()), Some("Headset Microphone (USB Audio)"));
        let ins = [dev("Headset Microphone (USB Audio)", false), dev("Microphone (Realtek(R) Audio)", true)];
        assert_eq!(pick_real_mic(&ins).map(|d| d.name.as_str()), Some("Microphone (Realtek(R) Audio)"));
        assert!(pick_real_mic(&[dev("Stereo Mix (Realtek)", true)]).is_none());
    }
}
