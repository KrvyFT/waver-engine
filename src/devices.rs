use cpal::traits::{DeviceTrait, HostTrait};
use waver_core::{AudioBackend, AudioCatalog, AudioDevice, AudioSelection};

pub fn default_selection() -> AudioSelection {
    AudioSelection {
        backend: cpal::default_host().id().name().into(),
        device: None,
    }
}

pub(crate) fn selected_host(selection: &AudioSelection) -> Result<cpal::Host, String> {
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name() == selection.backend)
        .ok_or_else(|| format!("音频后端 {} 不可用", selection.backend))?;
    cpal::host_from_id(id).map_err(|e| e.to_string())
}

pub fn audio_catalog() -> AudioCatalog {
    let mut catalog = AudioCatalog::default();
    for id in cpal::available_hosts() {
        let backend = id.name().to_owned();
        catalog.backends.push(AudioBackend {
            id: backend.clone(),
            label: if backend.eq_ignore_ascii_case("wasapi") {
                "普通设备 / WASAPI".into()
            } else {
                backend.clone()
            },
            unavailable_reason: None,
        });
        let host = match cpal::host_from_id(id) {
            Ok(host) => host,
            Err(e) => {
                catalog.errors.push(format!("{backend}: {e}"));
                continue;
            }
        };
        match host.output_devices() {
            Ok(devices) => {
                for device in devices {
                    let device_id = match device.id() {
                        Ok(id) => id.to_string(),
                        Err(e) => {
                            catalog.errors.push(format!("{backend}: {e}"));
                            continue;
                        }
                    };
                    catalog.devices.push(AudioDevice {
                        id: device_id,
                        backend: backend.clone(),
                        label: device
                            .description()
                            .map(|d| d.name().to_owned())
                            .unwrap_or_else(|_| device.to_string()),
                    });
                }
            }
            Err(e) => catalog.errors.push(format!("{backend}: {e}")),
        }
    }
    #[cfg(target_os = "windows")]
    if !catalog
        .backends
        .iter()
        .any(|backend| backend.id.eq_ignore_ascii_case("asio"))
    {
        catalog.backends.push(AudioBackend {
            id: "ASIO".into(),
            label: "ASIO".into(),
            unavailable_reason: Some(
                if cfg!(feature = "asio") {
                    "未检测到可用的 ASIO 后端，请安装声卡厂商的 ASIO 驱动。"
                } else {
                    "当前版本未启用 ASIO；需要支持 ASIO 的版本和声卡驱动。"
                }
                .into(),
            ),
        });
    }
    catalog
}
