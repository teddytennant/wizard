//! Which microphone a call (or dictation) records from. The system default
//! is usually right, but on Linux desktops it can be a sink's *monitor*:
//! with output-only Bluetooth headphones connected, PipeWire may make their
//! monitor the default source, and recording it captures whatever is
//! playing, including the call's own voice. Monitors and unplugged jacks are
//! skipped for the best real microphone instead.
use cpal::traits::HostTrait;

/// `preferred` (a `cpal::DeviceId` string) when it is connected, else the
/// default input when it is a real microphone, else the best real one.
pub fn input_device(host: &cpal::Host, preferred: Option<&str>) -> Option<cpal::Device> {
    if let Some(device) = preferred
        .and_then(|id| id.parse::<cpal::DeviceId>().ok())
        .and_then(|id| host.device_by_id(&id))
    {
        return Some(device);
    }
    #[cfg(target_os = "linux")]
    if let Some(name) = pulse::best_source() {
        let id = cpal::DeviceId::new(cpal::HostId::PulseAudio, name);
        if let Some(device) = host.device_by_id(&id) {
            return Some(device);
        }
    }
    host.default_input_device()
}

/// Whether a device id names a monitor of an output rather than a microphone.
pub fn is_monitor(id: &str) -> bool {
    id.ends_with(".monitor")
}

#[cfg(target_os = "linux")]
mod pulse {
    use pulseaudio::protocol::{self, SourceInfo, port_info::PortAvailable};

    /// The default source when it records something real, else the highest
    /// priority source that is no monitor and has its jack plugged in (or no
    /// jack detection). `None` without a PulseAudio-compatible server.
    pub(super) fn best_source() -> Option<String> {
        pulseaudio::socket_path_from_env()?;
        let client = pulseaudio::Client::from_env(c"wizard-gui").ok()?;
        futures::executor::block_on(async {
            let sources = client.list_sources().await.ok()?;
            let default = client
                .source_info_by_name(protocol::DEFAULT_SOURCE.to_owned())
                .await
                .ok()
                .map(|info| info.name);
            choose(&sources, default.as_deref().and_then(|n| n.to_str().ok()))
        })
    }

    fn real(source: &SourceInfo) -> bool {
        source.monitor_of_sink_index.is_none()
            && source
                .ports
                .get(source.active_port)
                .is_none_or(|port| port.available != PortAvailable::No)
    }

    fn priority(source: &SourceInfo) -> u32 {
        source
            .ports
            .get(source.active_port)
            .map_or(0, |port| port.priority)
    }

    pub(super) fn choose(sources: &[SourceInfo], default: Option<&str>) -> Option<String> {
        let name = |s: &SourceInfo| s.name.to_str().ok().map(str::to_owned);
        if let Some(source) = sources
            .iter()
            .find(|s| default.is_some_and(|d| s.name.to_str() == Ok(d)))
            .filter(|s| real(s))
        {
            return name(source);
        }
        sources
            .iter()
            .filter(|s| real(s))
            // A headset's own microphone, then the highest-priority input.
            .max_by_key(|s| {
                let bluetooth = s.name.to_str().is_ok_and(|n| n.starts_with("bluez_input."));
                (bluetooth, priority(s))
            })
            .and_then(name)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pulseaudio::protocol::port_info::{PortDirection, PortInfo};
        use std::ffi::CString;

        fn source(
            name: &str,
            monitor: bool,
            available: PortAvailable,
            priority: u32,
        ) -> SourceInfo {
            SourceInfo {
                name: CString::new(name).unwrap(),
                monitor_of_sink_index: monitor.then_some(1),
                ports: vec![PortInfo {
                    name: CString::new("port").unwrap(),
                    port_type: Default::default(),
                    description: None,
                    dir: PortDirection::Input,
                    priority,
                    available,
                    availability_group: None,
                }],
                active_port: 0,
                ..Default::default()
            }
        }

        #[test]
        fn a_monitor_default_falls_to_the_plugged_in_microphone() {
            let sources = [
                source("bluez_output.AA.1.monitor", true, PortAvailable::Unknown, 0),
                source(
                    "alsa_input.HiFi__Mic2__source",
                    false,
                    PortAvailable::No,
                    200,
                ),
                source(
                    "alsa_input.HiFi__Mic1__source",
                    false,
                    PortAvailable::Unknown,
                    100,
                ),
            ];
            assert_eq!(
                choose(&sources, Some("bluez_output.AA.1.monitor")).as_deref(),
                Some("alsa_input.HiFi__Mic1__source")
            );
        }

        #[test]
        fn a_real_default_wins_and_a_headset_beats_the_laptop() {
            let sources = [
                source(
                    "alsa_input.HiFi__Mic1__source",
                    false,
                    PortAvailable::Unknown,
                    100,
                ),
                source("bluez_input.AA.0", false, PortAvailable::Unknown, 10),
            ];
            assert_eq!(
                choose(&sources, Some("alsa_input.HiFi__Mic1__source")).as_deref(),
                Some("alsa_input.HiFi__Mic1__source")
            );
            assert_eq!(choose(&sources, None).as_deref(), Some("bluez_input.AA.0"));
        }
    }
}
