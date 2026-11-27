//! uinput playback.

use evdev::uinput::VirtualDevice as Uinput;
use evdev::{
    AbsInfo as EvAbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputId, KeyCode, MiscCode,
    PropType, RelativeAxisCode, SwitchCode, UinputAbsSetup,
};

use super::{DeviceError, VIRTUAL_DEVICE_NAME};
use crate::caps::Capabilities;
use crate::event::{EV_KEY, EV_MSC, EV_REL, EV_SW, InputEvent};

/// A uinput device that plays frames. The kernel destroys it when this is
/// dropped (its file descriptor closes), releasing anything held on it.
#[derive(Debug)]
pub struct VirtualDevice {
    dev: Uinput,
    caps: Capabilities,
    buf: Vec<evdev::InputEvent>,
}

impl VirtualDevice {
    /// Creates a device that can emit everything in `caps`. Event types a
    /// uinput device can't be set up for here (LEDs, sounds, force
    /// feedback, `EV_REP`) are ignored.
    pub fn create(caps: &Capabilities) -> Result<Self, DeviceError> {
        let id = InputId::new(BusType::BUS_VIRTUAL, 0x6e78, 0x0001, 1);
        let mut builder = Uinput::builder()
            .map_err(DeviceError::Create)?
            .name(VIRTUAL_DEVICE_NAME)
            .input_id(id);
        for (kind, codes) in caps.types() {
            let codes = codes.iter().copied();
            builder = match kind {
                EV_KEY => builder.with_keys(&codes.map(KeyCode).collect::<AttributeSet<_>>()),
                EV_REL => builder
                    .with_relative_axes(&codes.map(RelativeAxisCode).collect::<AttributeSet<_>>()),
                EV_MSC => builder.with_msc(&codes.map(MiscCode).collect::<AttributeSet<_>>()),
                EV_SW => builder.with_switches(&codes.map(SwitchCode).collect::<AttributeSet<_>>()),
                _ => Ok(builder), // EV_ABS below; anything else unsupported
            }
            .map_err(DeviceError::Create)?;
        }
        for (code, info) in caps.abs() {
            let info = EvAbsInfo::new(
                0,
                info.minimum,
                info.maximum,
                info.fuzz,
                info.flat,
                info.resolution,
            );
            builder = builder
                .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode(code), info))
                .map_err(DeviceError::Create)?;
        }
        let props: AttributeSet<PropType> = caps.properties().map(PropType).collect();
        if props.iter().next().is_some() {
            builder = builder
                .with_properties(&props)
                .map_err(DeviceError::Create)?;
        }
        let dev = builder.build().map_err(DeviceError::Create)?;
        Ok(Self {
            dev,
            caps: caps.clone(),
            buf: Vec::new(),
        })
    }

    /// What this device was created with.
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Plays one frame, followed by a `SYN_REPORT`. Frame boundaries in
    /// `frame` are skipped. Events the device can't emit are dropped by
    /// the kernel. An empty frame plays nothing.
    pub fn play(&mut self, frame: &[InputEvent]) -> Result<(), DeviceError> {
        self.buf.clear();
        self.buf.extend(
            frame
                .iter()
                .filter(|ev| !ev.is_frame_boundary())
                .map(|ev| evdev::InputEvent::new(ev.kind, ev.code, ev.value)),
        );
        if self.buf.is_empty() {
            return Ok(());
        }
        self.dev.emit(&self.buf).map_err(DeviceError::Write)
    }
}
