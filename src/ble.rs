use anyhow::anyhow;
use once_cell::sync::Lazy;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use serde::{Deserialize, Deserializer};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

static MGR: Lazy<PacketManager> = Lazy::new(PacketManager::new);

#[derive(Clone, PartialEq, Eq)]
pub struct HexBytes(Vec<u8>);

impl std::fmt::Debug for HexBytes {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.write_fmt(format_args!("{:02X?}", self.0))
    }
}

#[allow(clippy::type_complexity)]
pub struct PacketCodec {
    encode: Box<dyn Fn(&dyn Any) -> anyhow::Result<Vec<u8>> + Sync + Send>,
    decode: Box<dyn Fn(&[u8]) -> anyhow::Result<GoveeBlePacket> + Sync + Send>,
    supported_skus: &'static [&'static str],
    type_id: TypeId,
}

impl PacketCodec {
    pub fn new<T: 'static>(
        supported_skus: &'static [&'static str],
        encode: impl Fn(&T) -> anyhow::Result<Vec<u8>> + 'static + Sync + Send,
        decode: impl Fn(&[u8]) -> anyhow::Result<GoveeBlePacket> + 'static + Sync + Send,
    ) -> Self {
        Self {
            encode: Box::new(move |any| {
                let type_id = TypeId::of::<T>();
                let value = any.downcast_ref::<T>().ok_or_else(|| {
                    anyhow!("cannot downcast to {type_id:?} in PacketCodec encoder")
                })?;
                (encode)(value)
            }),
            decode: Box::new(decode),
            supported_skus,
            type_id: TypeId::of::<T>(),
        }
    }
}

pub struct PacketManager {
    codec_by_sku: Mutex<HashMap<String, HashMap<TypeId, Arc<PacketCodec>>>>,
    all_codecs: Vec<Arc<PacketCodec>>,
}

impl PacketManager {
    fn map_for_sku(&self, sku: &str) -> MappedMutexGuard<'_, HashMap<TypeId, Arc<PacketCodec>>> {
        MutexGuard::map(self.codec_by_sku.lock(), |codecs| {
            codecs.entry(sku.to_string()).or_insert_with(|| {
                let mut map = HashMap::new();

                for codec in &self.all_codecs {
                    if codec.supported_skus.contains(&sku)
                        && map.insert(codec.type_id, codec.clone()).is_some()
                    {
                        eprintln!("Conflicting PacketCodecs for {sku} {:?}", codec.type_id);
                    }
                }

                map
            })
        })
    }

    fn resolve_by_sku(&self, sku: &str, type_id: &TypeId) -> anyhow::Result<Arc<PacketCodec>> {
        let map = self.map_for_sku(sku);

        map.get(type_id)
            .cloned()
            .ok_or_else(|| anyhow!("sku {sku} has no codec for type {type_id:?}"))
    }

    pub fn decode_for_sku(&self, sku: &str, data: &[u8]) -> GoveeBlePacket {
        let map = self.map_for_sku(sku);

        for codec in map.values() {
            if let Ok(value) = (codec.decode)(data) {
                return value;
            }
        }

        GoveeBlePacket::Generic(HexBytes(data.to_vec()))
    }

    pub fn encode_for_sku<T: 'static>(&self, sku: &str, value: &T) -> anyhow::Result<Vec<u8>> {
        let type_id = TypeId::of::<T>();
        let codec = self.resolve_by_sku(sku, &type_id)?;

        (codec.encode)(value)
    }

    pub fn new() -> Self {
        let mut all_codecs = vec![];

        macro_rules! encode_body {
            // Tail case: nothing to do
            ($target:expr,$input:expr,) => {};

            // Match a constant byte; emit it
            ($target:expr,$input:expr, $expected:literal, $($tail:tt)*) => {
                    $target.push($expected);
                    encode_body!($target, $input, $($tail)*);
            };

            // Match a field; emit it from the struct
            ($target:expr, $input:expr, $field_name:ident, $($tail:tt)*) => {
                    $input.$field_name.encode_param($target);
                    encode_body!($target, $input, $($tail)*);
            };
        }

        macro_rules! decode_body {
            // Tail case; verify that remaining bytes are zero
            ($target:expr, $data:expr,) => {
                while !$data.is_empty() {
                    anyhow::ensure!($data[0] == 0);
                    $data = &$data[1..];
                }
            };

            // Match a constant byte; check that it is what we expect
            ($target:expr, $data:expr, $expected:literal, $($tail:tt)*) => {
                    let maybe_byte = $data.get(0);
                    anyhow::ensure!(maybe_byte == Some(&$expected),"expected {} but got {maybe_byte:?}", $expected);
                    $data = &$data[1..];
                    decode_body!($target, $data, $($tail)*);
            };

            // Match a field; parse it into the struct
            ($target:expr, $data:expr, $field_name:ident, $($tail:tt)*) => {
                    let remain = $target.$field_name.decode_param($data)?;
                    $data = remain;
                    decode_body!($target, $data, $($tail)*);
            };
        }

        /// Helper for defining a PacketCodec.
        /// The first param is the list of SKUs which are known to support
        /// this packet.
        /// The second parameter is the name of the type which will be
        /// encoded into raw bytes when encoding. It must impl Default.
        /// The third parameter is the name of the GoveeBlePacket enum
        /// variant that holds that type.
        /// The subsequent parameters are rules that match the bytes
        /// in the packet when decoding, or form the bytes in the packet
        /// when encoding. They are listed in the same sequence that they
        /// have in the packet.
        macro_rules! packet {
            ($skus:expr, $struct:ident, $variant:ident, $($body:tt)*) => {
                PacketCodec::new(
                    $skus,
                    |input_value: &$struct| {
                        let mut bytes = vec![];
                        encode_body!(&mut bytes, input_value, $($body)*);
                        Ok(finish(bytes))
                    },
                    |data| {
                        let mut data = &data[0..data.len().saturating_sub(1)];
                        let mut value = $struct::default();
                        decode_body!(&mut value, data, $($body)*);
                        Ok(GoveeBlePacket::$variant(value))
                    }
                )
            }
        }

        all_codecs.push(packet!(
            &["H7160"],
            SetHumidifierMode,
            SetHumidifierMode,
            0x33,
            0x05,
            mode,
            param,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            NotifyHumidifierMode,
            NotifyHumidifierMode,
            0xaa,
            0x05,
            0x00,
            mode,
            param,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            HumidifierAutoMode,
            NotifyHumidifierAutoMode,
            0xaa,
            0x05,
            0x03,
            target_humidity,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            NotifyHumidifierNightlightParams,
            NotifyHumidifierNightlight,
            0xaa,
            0x1b,
            on,
            brightness,
            r,
            g,
            b,
        ));
        all_codecs.push(packet!(
            &["H7160"],
            SetHumidifierNightlightParams,
            SetHumidifierNightlight,
            0x33,
            0x1b,
            on,
            brightness,
            r,
            g,
            b,
        ));
        all_codecs.push(PacketCodec::new(
            &["Generic:Light"],
            SetSceneCode::encode,
            SetSceneCode::decode,
        ));

        all_codecs.push(packet!(
            &["Generic:Light"],
            SetDevicePower,
            SetDevicePower,
            0x33,
            0x01,
            on,
        ));

        all_codecs.push(packet!(
            &[GENERIC_LIGHT],
            SetDeviceBrightness,
            SetDeviceBrightness,
            0x33,
            0x04,
            percent,
        ));

        all_codecs.push(packet!(
            &[GENERIC_LIGHT],
            SetDeviceColorRgb,
            SetDeviceColorRgb,
            0x33,
            0x05,
            0x0d,
            r,
            g,
            b,
        ));

        // Colour temperature reuses the colour opcode with the RGB field pinned
        // to ff ff ff as a "white mode" marker. The official app additionally
        // appends an RGB rendering of the temperature, which both reference
        // implementations omit without ill effect, so we omit it too rather than
        // invent values we cannot verify.
        all_codecs.push(packet!(
            &[GENERIC_LIGHT],
            SetDeviceColorTemperature,
            SetDeviceColorTemperature,
            0x33,
            0x05,
            0x0d,
            0xff,
            0xff,
            0xff,
            kelvin,
        ));

        // Notifications get hand-written decoders rather than the packet! macro
        // because the macro insists that every trailing byte is zero. That holds
        // for frames we generate, but we have no such guarantee for whatever a
        // device sends back, and rejecting an otherwise valid status update over
        // a stray padding byte would be a poor trade.
        all_codecs.push(PacketCodec::new(
            &[GENERIC_LIGHT],
            |value: &NotifyDevicePower| Ok(finish(vec![0xaa, 0x01, btoi(value.on)])),
            |data| {
                let body = notification_body(data, &[0xaa, 0x01])?;
                let on = *body.first().ok_or_else(|| anyhow!("EOF"))?;
                Ok(GoveeBlePacket::NotifyDevicePower(NotifyDevicePower {
                    on: itob(&on),
                }))
            },
        ));

        all_codecs.push(PacketCodec::new(
            &[GENERIC_LIGHT],
            |value: &NotifyDeviceBrightness| Ok(finish(vec![0xaa, 0x04, value.percent])),
            |data| {
                let body = notification_body(data, &[0xaa, 0x04])?;
                let percent = *body.first().ok_or_else(|| anyhow!("EOF"))?;
                Ok(GoveeBlePacket::NotifyDeviceBrightness(
                    NotifyDeviceBrightness { percent },
                ))
            },
        ));

        all_codecs.push(PacketCodec::new(
            &[GENERIC_LIGHT],
            |value: &NotifyDeviceColor| {
                let [hi, lo] = value.kelvin.0.to_be_bytes();
                Ok(finish(vec![
                    0xaa, 0x05, value.mode, value.r, value.g, value.b, hi, lo,
                ]))
            },
            |data| {
                let body = notification_body(data, &[0xaa, 0x05])?;
                anyhow::ensure!(
                    body.len() >= 6,
                    "colour notification is too short: {} bytes",
                    body.len()
                );
                // `aa 05 15` is the segment family, not a colour. It fits this
                // layout well enough to be mistaken for one — an H6072's
                // `aa 05 15 01` decoded as colour rgb(1,0,0), which over
                // Bluetooth would have been written into the device's state as
                // near-black.
                anyhow::ensure!(
                    body[0] != SEGMENT_MODE,
                    "{SEGMENT_MODE:#04x} is the segment mode, not a colour"
                );
                Ok(GoveeBlePacket::NotifyDeviceColor(NotifyDeviceColor {
                    mode: body[0],
                    r: body[1],
                    g: body[2],
                    b: body[3],
                    kelvin: OptionalKelvin(u16::from_be_bytes([body[4], body[5]])),
                }))
            },
        ));

        all_codecs.push(PacketCodec::new(
            &[GENERIC_LIGHT],
            |value: &SetSegmentColorRgb| {
                anyhow::ensure!(
                    value.mask != 0,
                    "a segment colour command that names no segment does nothing"
                );
                anyhow::ensure!(
                    value.mask >> (SEGMENT_MASK_BYTES * 8) == 0,
                    "segment mask {:#x} does not fit in {SEGMENT_MASK_BYTES} bytes",
                    value.mask
                );

                let mut bytes = vec![0x33, 0x05, 0x15, 0x01, value.r, value.g, value.b];
                // Bytes 7..=11 must stay zero. A non-zero byte 7 was measured
                // to blank three segments while the mask named one, so they are
                // not spare room.
                bytes.resize(SEGMENT_MASK_AT, 0);
                bytes.extend_from_slice(&value.mask.to_le_bytes()[..SEGMENT_MASK_BYTES]);
                Ok(finish(bytes))
            },
            |data| {
                let body = notification_body(data, &[0x33, 0x05, 0x15, 0x01])?;
                anyhow::ensure!(
                    body.len() >= SEGMENT_MASK_AT - 4 + SEGMENT_MASK_BYTES,
                    "segment colour command is too short: {} bytes",
                    body.len()
                );

                let mut mask = [0u8; 8];
                mask[..SEGMENT_MASK_BYTES]
                    .copy_from_slice(&body[SEGMENT_MASK_AT - 4..][..SEGMENT_MASK_BYTES]);

                Ok(GoveeBlePacket::SetSegmentColorRgb(SetSegmentColorRgb {
                    r: body[0],
                    g: body[1],
                    b: body[2],
                    mask: u64::from_le_bytes(mask),
                }))
            },
        ));

        all_codecs.push(PacketCodec::new(
            &[GENERIC_LIGHT],
            |value: &NotifySegmentColors| {
                let mut bytes = vec![0xaa, 0xa5, value.page];
                for segment in &value.segments {
                    bytes.extend_from_slice(&[segment.brightness, segment.r, segment.g, segment.b]);
                }
                Ok(finish(bytes))
            },
            |data| {
                // One page byte, then four bytes per segment.
                const PAGE_LEN: usize = 1 + MAX_SEGMENTS_PER_PAGE * 4;

                let body = notification_body(data, &[0xaa, 0xa5])?;
                anyhow::ensure!(
                    body.len() >= PAGE_LEN,
                    "segment notification is too short: {} bytes",
                    body.len()
                );

                let page = body[0];
                anyhow::ensure!(page > 0, "segment pages are numbered from 1, got {page}");

                let mut segments = [SegmentColor::default(); MAX_SEGMENTS_PER_PAGE];
                for (n, segment) in segments.iter_mut().enumerate() {
                    let at = 1 + n * 4;
                    *segment = SegmentColor {
                        brightness: body[at],
                        r: body[at + 1],
                        g: body[at + 2],
                        b: body[at + 3],
                    };
                }

                Ok(GoveeBlePacket::NotifySegmentColors(NotifySegmentColors {
                    page,
                    segments,
                }))
            },
        ));

        all_codecs.push(PacketCodec::new(
            &[GENERIC_LIGHT],
            |value: &NotifySegmentMode| Ok(finish(vec![0xaa, 0x05, SEGMENT_MODE, value.mode])),
            |data| {
                let body = notification_body(data, &[0xaa, 0x05, SEGMENT_MODE])?;
                Ok(GoveeBlePacket::NotifySegmentMode(NotifySegmentMode {
                    mode: body.first().copied().unwrap_or_default(),
                }))
            },
        ));

        Self {
            codec_by_sku: Mutex::new(HashMap::new()),
            all_codecs: all_codecs.into_iter().map(Arc::new).collect(),
        }
    }
}

pub trait DecodePacketParam {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]>;
    fn encode_param(&self, target: &mut Vec<u8>);
}

impl DecodePacketParam for u8 {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        *self = *data.first().ok_or_else(|| anyhow!("EOF"))?;
        Ok(&data[1..])
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.push(*self);
    }
}

impl DecodePacketParam for u16 {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        let lo = *data.first().ok_or_else(|| anyhow!("EOF"))?;
        let hi = *data.get(1).ok_or_else(|| anyhow!("EOF"))?;
        *self = ((hi as u16) << 8) | lo as u16;
        Ok(&data[2..])
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        let hi = (*self >> 8) as u8;
        let lo = (*self & 0xff) as u8;
        target.push(lo);
        target.push(hi);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SetHumidifierNightlightParams {
    pub on: bool,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub brightness: u8,
}

impl From<NotifyHumidifierNightlightParams> for SetHumidifierNightlightParams {
    fn from(val: NotifyHumidifierNightlightParams) -> Self {
        SetHumidifierNightlightParams {
            on: val.on,
            r: val.r,
            g: val.g,
            b: val.b,
            brightness: val.brightness,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NotifyHumidifierNightlightParams {
    pub on: bool,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub brightness: u8,
}

/// Data is offset by 128 with increments of 1%,
/// so 0% is 128, 100% is 228%
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetHumidity(u8);

impl From<TargetHumidity> for u8 {
    fn from(val: TargetHumidity) -> Self {
        val.0
    }
}

impl DecodePacketParam for TargetHumidity {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        self.0.decode_param(data)
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.push(self.0);
    }
}

impl TargetHumidity {
    pub fn as_percent(&self) -> u8 {
        self.0 & 0x7f
    }

    pub fn into_inner(self) -> u8 {
        self.0
    }

    pub fn from_percent(percent: u8) -> Self {
        Self(percent + 128)
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetHumidifierMode {
    pub mode: u8,
    pub param: u8,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct NotifyHumidifierMode {
    pub mode: u8,
    pub param: u8,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct HumidifierAutoMode {
    pub target_humidity: TargetHumidity,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetSceneCode {
    code: u16,
    scence_param: String,
}

impl SetSceneCode {
    pub fn new(code: u16, scence_param: String) -> Self {
        Self { code, scence_param }
    }

    /// For reference, see:
    /// <https://github.com/egold555/Govee-Reverse-Engineering/issues/11#issuecomment-2565692233>
    /// <https://github.com/AlgoClaw/Govee/blob/main/decoded/explanation>
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let bytes = data_encoding::BASE64.decode(self.scence_param.as_bytes())?;

        let mut data = vec![0xa3, 0x00, 0x01, 0x00 /* line count */, 0x02];
        let mut num_lines = 0u8;
        let mut last_line_marker = 1;

        for b in bytes {
            if data.len().is_multiple_of(19) {
                num_lines += 1;

                data.push(0xa3);
                last_line_marker = data.len();

                data.push(num_lines);
            }

            data.push(b);
        }
        // The last line uses 0xff as the indicator, rather than its line number
        data[last_line_marker] = 0xff;
        // back-patch the number of lines into the packet
        data[3] = num_lines + 1;

        // Now apply padding and checksums
        let mut padded = vec![];
        for chunk in data.chunks(19) {
            let mut padded_chunk = chunk.to_vec();
            padded_chunk = finish(padded_chunk);
            padded.append(&mut padded_chunk);
        }

        // and finally encode the scene code as the final packet "line"
        let hi = (self.code >> 8) as u8;
        let lo = (self.code & 0xff) as u8;
        padded.append(&mut finish(vec![0x33, 0x05, 0x04, lo, hi]));
        Ok(padded)
    }

    fn decode(_data: &[u8]) -> anyhow::Result<GoveeBlePacket> {
        anyhow::bail!("SetSceneCode::decode is not implemented");
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SetDevicePower {
    pub on: bool,
}

/// Codec key for the light command set. Govee's light protocol is consistent
/// across models, so rather than enumerating SKUs we register the light packets
/// under this synthetic key and address it explicitly at the call site.
pub const GENERIC_LIGHT: &str = "Generic:Light";

/// Brightness as a percentage. The device takes 0x00-0x64 for 0-100%.
///
/// Note that 0 switches the light off rather than dimming it, so callers that
/// mean "as dim as possible" must send 1.
///
/// The Python reference implementations get this wrong in an instructive way:
/// they render the percentage as a *decimal* string and then parse it as hex,
/// so 20% becomes 0x13 (19) instead of 0x14 (20). Do not reproduce that.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SetDeviceBrightness {
    pub percent: u8,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SetDeviceColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SetDeviceColorTemperature {
    pub kelvin: Kelvin,
}

/// Colour temperature in Kelvin, carried as a 16 bit big-endian value.
///
/// Zero is not a valid temperature: the device uses it to mean "no colour
/// temperature set, I am in RGB mode". Rejecting it while decoding is what keeps
/// a plain white RGB frame from also parsing as a colour temperature frame.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Kelvin(u16);

impl Kelvin {
    pub fn new(kelvin: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(kelvin != 0, "0K is not a valid colour temperature");
        Ok(Self(kelvin))
    }

    // Consumed by the BLE transport in a later milestone.
    #[allow(dead_code)]
    pub fn get(&self) -> u16 {
        self.0
    }
}

impl DecodePacketParam for Kelvin {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        let hi = *data.first().ok_or_else(|| anyhow!("EOF"))?;
        let lo = *data.get(1).ok_or_else(|| anyhow!("EOF"))?;
        *self = Kelvin::new(u16::from_be_bytes([hi, lo]))?;
        Ok(&data[2..])
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.extend_from_slice(&self.0.to_be_bytes());
    }
}

/// The colour temperature field of a status notification, where zero is
/// meaningful: it tells us the device is showing an RGB colour rather than white.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct OptionalKelvin(u16);

impl OptionalKelvin {
    // Consumed by the BLE transport in a later milestone.
    #[allow(dead_code)]
    pub fn get(&self) -> Option<u16> {
        (self.0 != 0).then_some(self.0)
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifyDevicePower {
    pub on: bool,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifyDeviceBrightness {
    pub percent: u8,
}

/// Reply to a colour query.
///
/// `kelvin` decides how to read this: a non-zero value means the device is in
/// colour temperature mode and the RGB fields are only its rendering of that
/// white point; zero means the RGB fields are the actual colour.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifyDeviceColor {
    /// Purpose not yet understood; queries send 0x01 here and devices echo
    /// varying values back. Preserved so we can learn from real traffic.
    pub mode: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub kelvin: OptionalKelvin,
}

/// The `05 15` family: segment colour when written, and a marker in a status
/// report that the device is addressed as segments at all.
pub const SEGMENT_MODE: u8 = 0x15;

/// A device saying it has segments.
///
/// Observed in the status of every segmented device on one account — H7020,
/// H6072, H6054, H60B2, H7093 — and in none of the unsegmented ones. That makes
/// it a better answer to "does this have segments?" than Govee's metadata,
/// which omits it for an H6054 entirely, and better than a model list, which
/// this project has already watched go stale once (§3.1).
///
/// The payload byte is `00` on some devices and `01` on others with no pattern
/// we can see, so it is carried but not interpreted.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifySegmentMode {
    pub mode: u8,
}

/// Where the segment bitmask starts in a `33 05 15 01` frame.
pub const SEGMENT_MASK_AT: usize = 12;

/// How many bytes of mask the frame has room for, between `SEGMENT_MASK_AT`
/// and the checksum. Bits up to 24 are confirmed against hardware; the rest is
/// what the frame allows.
pub const SEGMENT_MASK_BYTES: usize = 7;

/// Set the colour of individual segments.
///
/// `mask` is a little-endian bitfield: bit N addresses segment N, and segments
/// it does not name keep the colour they had. Reverse-engineered on 2026-08-25
/// against an H6072 and an H7020, using the `aa a5` pages as the oracle. Bits
/// 0, 3, 5, 15, 16 and 24 were each confirmed individually, so the mask is at
/// least four bytes wide.
///
/// Bytes 7..12 are not free padding and have to stay zero: setting byte 7 to
/// `0x0a` turned segments 0, 1 and 2 black while the mask named only segment
/// 0, so a value there changes how the rest of the frame is read.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SetSegmentColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub mask: u64,
}

impl SetSegmentColorRgb {
    /// Build a command for a set of segment indices.
    pub fn for_segments(
        segments: impl IntoIterator<Item = u32>,
        (r, g, b): (u8, u8, u8),
    ) -> anyhow::Result<Self> {
        let mut mask = 0u64;
        for segment in segments {
            let bit = u64::from(segment);
            anyhow::ensure!(
                bit < (SEGMENT_MASK_BYTES * 8) as u64,
                "segment {segment} is past the {} this frame can address",
                SEGMENT_MASK_BYTES * 8
            );
            mask |= 1 << bit;
        }
        Ok(Self { r, g, b, mask })
    }
}

/// Most segment groups an `aa a5` page can carry: the seventeen payload bytes
/// hold a page number and four groups of four.
pub const MAX_SEGMENTS_PER_PAGE: usize = 4;

/// What most SKUs actually use. H6072, H7020 and H60B2 send three groups and
/// leave the last four bytes zero; an H6054 fills all four. See §17.
pub const SEGMENTS_PER_PAGE: usize = 3;

/// One segment's colour, as it appears inside an `aa a5` page.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SegmentColor {
    /// Reported per segment, but not on the device's brightness scale: a lamp
    /// reporting 60% overall reports 0x5f here. Kept as the raw byte until we
    /// know what it measures.
    pub brightness: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Per-segment colours, reported in pages of three.
///
/// This is the only source of segment state we have: the Platform API answers
/// `segmentedColorRgb` with an empty string, while the device volunteers these
/// frames inside its reply to an AWS IoT `status` request. A page always
/// carries three slots even when the device has fewer segments left to
/// describe, so the trailing slots of the last page can be filler.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NotifySegmentColors {
    /// 1-based. Which segments it describes depends on the stride, which is a
    /// property of the device rather than of the page — see `groups_used`.
    pub page: u8,
    pub segments: [SegmentColor; MAX_SEGMENTS_PER_PAGE],
}

impl NotifySegmentColors {
    /// Index of the first segment this page describes, given the device's
    /// stride.
    pub fn first_segment_index(&self, stride: usize) -> u32 {
        u32::from(self.page.saturating_sub(1)) * stride as u32
    }

    /// How many groups this page appears to use.
    ///
    /// A three-group device leaves the last four bytes zero, so a non-zero
    /// fourth group means four. Read one page at a time this is *nearly* safe:
    /// a segment switched off is set to black (§15), which looks exactly like
    /// padding. Deciding from a whole status batch and keeping the answer —
    /// which `Device::set_segment_colors` does — contains that.
    pub fn groups_used(&self) -> usize {
        if self.segments[MAX_SEGMENTS_PER_PAGE - 1] == SegmentColor::default() {
            SEGMENTS_PER_PAGE
        } else {
            MAX_SEGMENTS_PER_PAGE
        }
    }
}

/// Validate a notification frame and return the bytes following `prefix`,
/// excluding the trailing checksum.
fn notification_body(data: &[u8], prefix: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        data.len() == 20,
        "expected a 20 byte frame, got {}",
        data.len()
    );
    let expected = calculate_checksum(&data[0..19]);
    anyhow::ensure!(
        data[19] == expected,
        "checksum mismatch: got {:02x}, expected {expected:02x}",
        data[19]
    );
    anyhow::ensure!(data.starts_with(prefix), "unexpected header");
    Ok(data[prefix.len()..19].to_vec())
}

/// Decode a frame a device sent us.
///
/// Unlike `Base64HexBytes::with_bytes`, this does not re-pad or re-checksum:
/// the bytes are exactly what arrived over the air.
pub fn decode_notification(sku: &str, data: &[u8]) -> GoveeBlePacket {
    MGR.decode_for_sku(sku, data)
}

/// Ask the device to report its power state.
// Consumed by the BLE transport in a later milestone.
#[allow(dead_code)]
pub fn query_device_power() -> Base64HexBytes {
    Base64HexBytes::with_bytes(vec![0xaa, 0x01])
}

/// Ask the device to report its brightness.
// Consumed by the BLE transport in a later milestone.
#[allow(dead_code)]
pub fn query_device_brightness() -> Base64HexBytes {
    Base64HexBytes::with_bytes(vec![0xaa, 0x04])
}

/// Encode a light command into the raw 20 bytes that travel over either
/// channel — the radio directly, or base64-wrapped as `ptReal` over LAN and
/// AWS IoT. Every light codec lives under `GENERIC_LIGHT` rather than a SKU,
/// which is easy to get wrong at the call site; this says it once.
pub fn encode_for_generic_light<T: 'static>(value: &T) -> anyhow::Result<Vec<u8>> {
    MGR.encode_for_sku(GENERIC_LIGHT, value)
}

/// Ask the device to report one page of segment colours.
///
/// Confirmed on an H6072 on 2026-08-25: sending `aa a5 02` came back as
/// `aa a5 02 <three segment groups>` two seconds before any status request, so
/// this is a query in its own right and not merely the shape of a status
/// reply. It makes segment state live over Bluetooth rather than tied to the
/// AWS IoT poll interval, and it is the only way a Bluetooth-only segmented
/// device can tell us how many segments it has.
///
/// Pages are numbered from 1; page P covers segments from
/// `(P - 1) * stride`, where the stride is the device's own — see
/// `NotifySegmentColors::groups_used`.
// Verified against hardware but not yet wired up: reading segments back in the
// session that wrote them needs a `Query` variant in the BLE scheduler, which
// is its own change. Kept here so the measurement is not lost.
#[allow(dead_code)]
pub fn query_segment_colors(page: u8) -> Base64HexBytes {
    Base64HexBytes::with_bytes(vec![0xaa, 0xa5, page.max(1)])
}

/// Ask the device to report its colour and colour temperature.
///
/// Unlike the other queries this one carries 0x01 rather than 0x00 in the third
/// byte; devices do not answer without it.
// Consumed by the BLE transport in a later milestone.
#[allow(dead_code)]
pub fn query_device_color() -> Base64HexBytes {
    Base64HexBytes::with_bytes(vec![0xaa, 0x05, 0x01])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoveeBlePacket {
    Generic(HexBytes),
    #[allow(unused)] // can remove if/when SetSceneCode::decode has an impl
    SetSceneCode(SetSceneCode),
    SetDevicePower(SetDevicePower),
    SetDeviceBrightness(SetDeviceBrightness),
    SetDeviceColorRgb(SetDeviceColorRgb),
    SetDeviceColorTemperature(SetDeviceColorTemperature),
    NotifyDevicePower(NotifyDevicePower),
    NotifyDeviceBrightness(NotifyDeviceBrightness),
    NotifyDeviceColor(NotifyDeviceColor),
    NotifySegmentColors(NotifySegmentColors),
    SetSegmentColorRgb(SetSegmentColorRgb),
    NotifySegmentMode(NotifySegmentMode),
    SetHumidifierNightlight(SetHumidifierNightlightParams),
    NotifyHumidifierMode(NotifyHumidifierMode),
    SetHumidifierMode(SetHumidifierMode),
    NotifyHumidifierAutoMode(HumidifierAutoMode),
    NotifyHumidifierNightlight(NotifyHumidifierNightlightParams),
}

#[derive(Debug)]
pub struct Base64HexBytes(HexBytes);

impl Base64HexBytes {
    pub fn decode_for_sku(&self, sku: &str) -> GoveeBlePacket {
        MGR.decode_for_sku(sku, &self.0 .0)
    }

    pub fn encode_for_sku<T: 'static>(sku: &str, value: &T) -> anyhow::Result<Self> {
        MGR.encode_for_sku(sku, value)
            .map(|bytes| Base64HexBytes(HexBytes(bytes)))
    }

    pub fn base64(&self) -> Vec<String> {
        let mut result = vec![];
        for chunk in self.0 .0.chunks(20) {
            result.push(data_encoding::BASE64.encode(chunk));
        }
        result
    }

    pub fn with_bytes(bytes: Vec<u8>) -> Self {
        Self(HexBytes(finish(bytes)))
    }
}

impl<'de> Deserialize<'de> for Base64HexBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, <D as Deserializer<'de>>::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;
        let encoded = String::deserialize(deserializer)?;
        let decoded = data_encoding::BASE64
            .decode(encoded.as_ref())
            .map_err(|e| D::Error::custom(format!("{e:#}")))?;
        Ok(Self(HexBytes(decoded)))
    }
}

fn calculate_checksum(data: &[u8]) -> u8 {
    let mut checksum: u8 = 0;
    for &b in data {
        checksum ^= b;
    }
    checksum
}

fn finish(mut data: Vec<u8>) -> Vec<u8> {
    let checksum = calculate_checksum(&data);
    data.resize(19, 0);
    data.push(checksum);
    data
}

impl DecodePacketParam for bool {
    fn decode_param<'a>(&mut self, data: &'a [u8]) -> anyhow::Result<&'a [u8]> {
        let mut byte = 0u8;
        let remain = byte.decode_param(data)?;
        *self = itob(&byte);
        Ok(remain)
    }

    fn encode_param(&self, target: &mut Vec<u8>) {
        target.push(btoi(*self));
    }
}

fn btoi(on: bool) -> u8 {
    if on {
        1
    } else {
        0
    }
}

fn itob(i: &u8) -> bool {
    *i != 0
}

impl GoveeBlePacket {}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn packet_manager() {
        assert_eq!(
            MGR.decode_for_sku(
                "H7160",
                &[0x33, 0x05, 0x01, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23]
            ),
            GoveeBlePacket::SetHumidifierMode(SetHumidifierMode {
                mode: 1,
                param: 0x20
            })
        );

        assert_eq!(
            MGR.encode_for_sku(
                "H7160",
                &SetHumidifierMode {
                    mode: 1,
                    param: 0x20
                }
            )
            .unwrap(),
            vec![0x33, 0x05, 0x01, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23]
        );
    }

    fn round_trip<T: 'static + std::fmt::Debug>(sku: &str, value: &T, expect: GoveeBlePacket) {
        let bytes = Base64HexBytes::encode_for_sku(sku, value).unwrap();
        let decoded = bytes.decode_for_sku(sku);
        assert_eq!(decoded, expect);
    }

    #[test]
    fn basic_round_trip() {
        round_trip(
            "Generic:Light",
            &SetDevicePower { on: true },
            GoveeBlePacket::SetDevicePower(SetDevicePower { on: true }),
        );
        round_trip(
            "H7160",
            &SetHumidifierNightlightParams {
                on: true,
                r: 255,
                g: 69,
                b: 42,
                brightness: 100,
            },
            GoveeBlePacket::SetHumidifierNightlight(SetHumidifierNightlightParams {
                on: true,
                r: 255,
                g: 69,
                b: 42,
                brightness: 100,
            }),
        );
    }

    #[test]
    fn scene_command() {
        const FOREST_SCENCE_PARAM: &str = "AyYAAQAKAgH/GQG0CgoCyBQF//8AAP//////AP//lP8AFAGWAAAAACMAAg8FAgH/FAH7AAAB+goEBP8AtP8AR///4/8AAAAAAAAAABoAAAABAgH/BQHIFBQC7hQBAP8AAAAAAAAAAA==";
        const FOREST_SCENE_CODE: u16 = 212;

        let command = SetSceneCode::new(FOREST_SCENE_CODE, FOREST_SCENCE_PARAM.to_string());

        let padded = command.encode().unwrap();

        println!("data is:");
        let mut hex = String::new();
        for (idx, b) in padded.iter().enumerate() {
            if idx % 20 == 0 && !hex.is_empty() {
                hex.push('\n');
            } else if !hex.is_empty() {
                hex.push(' ');
            }
            hex.push_str(&format!("{b:02x}"));
        }
        println!("{hex}");

        k9::snapshot!(
            hex,
            "
a3 00 01 07 02 03 26 00 01 00 0a 02 01 ff 19 01 b4 0a 0a d9
a3 01 02 c8 14 05 ff ff 00 00 ff ff ff ff ff 00 ff ff 94 12
a3 02 ff 00 14 01 96 00 00 00 00 23 00 02 0f 05 02 01 ff 0a
a3 03 14 01 fb 00 00 01 fa 0a 04 04 ff 00 b4 ff 00 47 ff b3
a3 04 ff e3 ff 00 00 00 00 00 00 00 00 1a 00 00 00 01 02 5d
a3 05 01 ff 05 01 c8 14 14 02 ee 14 01 00 ff 00 00 00 00 92
a3 ff 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 5c
33 05 04 d4 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 e6
"
        );
    }
    fn hex(bytes: &Base64HexBytes) -> String {
        bytes.0 .0.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn encode_light<T: 'static>(value: &T) -> String {
        hex(&Base64HexBytes::encode_for_sku(GENERIC_LIGHT, value).unwrap())
    }

    /// The power, brightness, RGB and 6500K expectations are frames captured
    /// from a real device, transcribed from the reverse-engineering
    /// repositories. The 2700K case is the same encoding at a second data
    /// point; only the app's longer variant of it was captured.
    #[test]
    fn light_frames_match_captured_traffic() {
        assert_eq!(
            encode_light(&SetDevicePower { on: true }),
            "3301010000000000000000000000000000000033"
        );
        assert_eq!(
            encode_light(&SetDevicePower { on: false }),
            "3301000000000000000000000000000000000032"
        );
        assert_eq!(
            encode_light(&SetDeviceBrightness { percent: 100 }),
            "3304640000000000000000000000000000000053"
        );
        assert_eq!(
            encode_light(&SetDeviceColorRgb {
                r: 0x8b,
                g: 0x00,
                b: 0xff
            }),
            "33050d8b00ff000000000000000000000000004f"
        );
        assert_eq!(
            encode_light(&SetDeviceColorTemperature {
                kelvin: Kelvin::new(6500).unwrap()
            }),
            "33050dffffff19640000000000000000000000b9"
        );
        assert_eq!(
            encode_light(&SetDeviceColorTemperature {
                kelvin: Kelvin::new(2700).unwrap()
            }),
            "33050dffffff0a8c000000000000000000000042"
        );
    }

    #[test]
    fn queries_match_captured_traffic() {
        assert_eq!(
            hex(&query_device_power()),
            "aa010000000000000000000000000000000000ab"
        );
        assert_eq!(
            hex(&query_device_brightness()),
            "aa040000000000000000000000000000000000ae"
        );
        assert_eq!(
            hex(&query_device_color()),
            "aa050100000000000000000000000000000000ae"
        );
    }

    #[test]
    fn light_packets_round_trip() {
        round_trip(
            GENERIC_LIGHT,
            &SetDeviceBrightness { percent: 42 },
            GoveeBlePacket::SetDeviceBrightness(SetDeviceBrightness { percent: 42 }),
        );
        round_trip(
            GENERIC_LIGHT,
            &SetDeviceColorRgb { r: 1, g: 2, b: 254 },
            GoveeBlePacket::SetDeviceColorRgb(SetDeviceColorRgb { r: 1, g: 2, b: 254 }),
        );
        round_trip(
            GENERIC_LIGHT,
            &SetDeviceColorTemperature {
                kelvin: Kelvin::new(4800).unwrap(),
            },
            GoveeBlePacket::SetDeviceColorTemperature(SetDeviceColorTemperature {
                kelvin: Kelvin::new(4800).unwrap(),
            }),
        );
        round_trip(
            GENERIC_LIGHT,
            &NotifyDevicePower { on: true },
            GoveeBlePacket::NotifyDevicePower(NotifyDevicePower { on: true }),
        );
        round_trip(
            GENERIC_LIGHT,
            &NotifyDeviceBrightness { percent: 7 },
            GoveeBlePacket::NotifyDeviceBrightness(NotifyDeviceBrightness { percent: 7 }),
        );
        round_trip(
            GENERIC_LIGHT,
            &NotifyDeviceColor {
                mode: 1,
                r: 10,
                g: 20,
                b: 30,
                kelvin: OptionalKelvin(0),
            },
            GoveeBlePacket::NotifyDeviceColor(NotifyDeviceColor {
                mode: 1,
                r: 10,
                g: 20,
                b: 30,
                kelvin: OptionalKelvin(0),
            }),
        );
    }

    /// A white RGB frame and a colour temperature frame share the same first six
    /// bytes. The only thing telling them apart is the Kelvin field, so a white
    /// RGB command must not come back as a colour temperature.
    #[test]
    fn white_rgb_is_not_mistaken_for_colour_temperature() {
        round_trip(
            GENERIC_LIGHT,
            &SetDeviceColorRgb {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
            GoveeBlePacket::SetDeviceColorRgb(SetDeviceColorRgb {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            }),
        );
    }

    #[test]
    fn zero_kelvin_is_rejected() {
        assert!(Kelvin::new(0).is_err());
    }

    /// We control the frames we send, but not what a device sends back. A status
    /// notification carrying unexpected trailing data must still be understood.
    #[test]
    fn notification_tolerates_trailing_bytes() {
        let mut frame = vec![0xaa, 0x05, 0x01, 0x8b, 0x00, 0xff, 0x12, 0xc0];
        frame.resize(19, 0);
        frame[18] = 0x42;
        frame.push(calculate_checksum(&frame));

        assert_eq!(
            MGR.decode_for_sku(GENERIC_LIGHT, &frame),
            GoveeBlePacket::NotifyDeviceColor(NotifyDeviceColor {
                mode: 0x01,
                r: 0x8b,
                g: 0x00,
                b: 0xff,
                kelvin: OptionalKelvin(4800),
            })
        );
    }

    #[test]
    fn notification_with_a_bad_checksum_is_rejected() {
        let mut frame = vec![0xaa, 0x01, 0x01];
        frame.resize(19, 0);
        frame.push(calculate_checksum(&frame) ^ 0xff);

        assert!(matches!(
            MGR.decode_for_sku(GENERIC_LIGHT, &frame),
            GoveeBlePacket::Generic(_)
        ));
    }

    #[test]
    fn optional_kelvin_distinguishes_rgb_from_white() {
        assert_eq!(OptionalKelvin(0).get(), None);
        assert_eq!(OptionalKelvin(2700).get(), Some(2700));
    }

    /// Every one of these was sent to a real H6072 or H7020 and the change
    /// confirmed by reading the `aa a5` pages back.
    #[test]
    fn segment_colour_commands_match_what_the_hardware_accepted() {
        let cases = [
            // segment 0 -> blue
            (
                vec![0u32],
                (0x00, 0x00, 0xff),
                "3305150100 00ff 0000000000 0100 0000000000 dc",
            ),
            // segments 3 and 5 -> white, leaving 4 alone
            (
                vec![3, 5],
                (0xff, 0xff, 0xff),
                "33051501ff ffff 0000000000 2800 0000000000 f5",
            ),
            // segment 15 -> red, the top bit of the first mask byte pair
            (
                vec![15],
                (0xff, 0x00, 0x00),
                "33051501ff 0000 0000000000 0080 0000000000 5d",
            ),
            // segment 16 -> green, which needs a third mask byte
            (
                vec![16],
                (0x00, 0xff, 0x00),
                "3305150100 ff00 0000000000 0000 0100000000 dc",
            ),
        ];

        for (segments, colour, expected) in cases {
            let value = SetSegmentColorRgb::for_segments(segments.clone(), colour).unwrap();
            assert_eq!(
                hex(&Base64HexBytes::encode_for_sku(GENERIC_LIGHT, &value).unwrap()),
                expected.replace(' ', ""),
                "encoding {segments:?}"
            );
            round_trip(
                GENERIC_LIGHT,
                &value,
                GoveeBlePacket::SetSegmentColorRgb(value),
            );
        }
    }

    /// Verbatim from an H6072 and an H60B2. Before the guard, the first of
    /// these decoded as a colour of rgb(1, 0, 0) — which the Bluetooth path
    /// would have written into the device state as near-black.
    #[test]
    fn the_segment_marker_is_not_mistaken_for_a_colour() {
        for (frame, mode) in [("aa051501", 1u8), ("aa051500", 0)] {
            let bytes = Base64HexBytes::with_bytes(
                (0..frame.len())
                    .step_by(2)
                    .map(|n| u8::from_str_radix(&frame[n..n + 2], 16).unwrap())
                    .collect(),
            );

            assert_eq!(
                MGR.decode_for_sku(GENERIC_LIGHT, &bytes.0 .0),
                GoveeBlePacket::NotifySegmentMode(NotifySegmentMode { mode }),
                "decoding {frame}"
            );
        }
    }

    /// And an ordinary colour report still decodes as one.
    #[test]
    fn a_colour_report_still_decodes_as_a_colour() {
        let bytes = Base64HexBytes::with_bytes(vec![0xaa, 0x05, 0x0d, 0x00, 0x00, 0xff]);

        assert_eq!(
            MGR.decode_for_sku(GENERIC_LIGHT, &bytes.0 .0),
            GoveeBlePacket::NotifyDeviceColor(NotifyDeviceColor {
                mode: 0x0d,
                r: 0,
                g: 0,
                b: 0xff,
                kelvin: OptionalKelvin(0),
            })
        );
    }

    /// The exact bytes an H6072 answered.
    #[test]
    fn the_segment_query_matches_what_the_device_answered() {
        assert_eq!(
            hex(&query_segment_colors(1)),
            "aaa501000000000000000000000000000000000e"
        );
        assert_eq!(
            hex(&query_segment_colors(2)),
            "aaa502000000000000000000000000000000000d"
        );

        // Page numbers start at 1; 0 would collide with page 1's segments.
        assert_eq!(hex(&query_segment_colors(0)), hex(&query_segment_colors(1)));
    }

    /// The frame has room for seven mask bytes and no more.
    #[test]
    fn a_segment_past_the_mask_is_refused() {
        assert!(SetSegmentColorRgb::for_segments([55], (1, 2, 3)).is_ok());
        assert!(SetSegmentColorRgb::for_segments([56], (1, 2, 3)).is_err());
    }

    /// Naming no segment would be a frame that quietly does nothing.
    #[test]
    fn an_empty_segment_mask_is_refused() {
        let empty = SetSegmentColorRgb::for_segments([], (1, 2, 3)).unwrap();
        assert!(Base64HexBytes::encode_for_sku(GENERIC_LIGHT, &empty).is_err());
    }

    fn segment(brightness: u8, r: u8, g: u8, b: u8) -> SegmentColor {
        SegmentColor {
            brightness,
            r,
            g,
            b,
        }
    }

    /// Captured from an H6072 whose eight segments were, from the bottom,
    /// red / green / yellow / green / magenta / green / cyan / green. The
    /// ninth slot of the last page does not exist on this lamp and carries
    /// filler.
    #[test]
    fn decodes_segment_colours_captured_from_a_device() {
        let cases = [
            (
                "aaa5015fff00005f00ff005fffff000000000051",
                NotifySegmentColors {
                    page: 1,
                    segments: [
                        segment(0x5f, 0xff, 0x00, 0x00),
                        segment(0x5f, 0x00, 0xff, 0x00),
                        segment(0x5f, 0xff, 0xff, 0x00),
                        SegmentColor::default(),
                    ],
                },
            ),
            (
                "aaa5025f00ff005fff00ff5f00ff000000000052",
                NotifySegmentColors {
                    page: 2,
                    segments: [
                        segment(0x5f, 0x00, 0xff, 0x00),
                        segment(0x5f, 0xff, 0x00, 0xff),
                        segment(0x5f, 0x00, 0xff, 0x00),
                        SegmentColor::default(),
                    ],
                },
            ),
            (
                "aaa5035f00ffff5f00ff002a5f5f5f0000000086",
                NotifySegmentColors {
                    page: 3,
                    segments: [
                        segment(0x5f, 0x00, 0xff, 0xff),
                        segment(0x5f, 0x00, 0xff, 0x00),
                        segment(0x2a, 0x5f, 0x5f, 0x5f),
                        SegmentColor::default(),
                    ],
                },
            ),
        ];

        for (frame, expected) in cases {
            let bytes: Vec<u8> = (0..frame.len())
                .step_by(2)
                .map(|n| u8::from_str_radix(&frame[n..n + 2], 16).unwrap())
                .collect();

            assert_eq!(
                MGR.decode_for_sku(GENERIC_LIGHT, &bytes),
                GoveeBlePacket::NotifySegmentColors(expected),
                "decoding {frame}"
            );

            // And the encoder reproduces the device's own bytes exactly.
            assert_eq!(
                hex(&Base64HexBytes::encode_for_sku(GENERIC_LIGHT, &expected).unwrap()),
                frame
            );
        }
    }

    /// Page numbers are 1-based; every page maps onto three consecutive
    /// segments, which is what lines the frames up with the `Segment {n:03}`
    /// entities.
    #[test]
    fn segment_pages_map_onto_segment_indices() {
        let page_of = |page| NotifySegmentColors {
            page,
            ..Default::default()
        };

        assert_eq!(page_of(1).first_segment_index(SEGMENTS_PER_PAGE), 0);
        assert_eq!(page_of(2).first_segment_index(SEGMENTS_PER_PAGE), 3);
        assert_eq!(page_of(3).first_segment_index(SEGMENTS_PER_PAGE), 6);

        // An H6054 packs four to a page, so page 2 starts at segment 4.
        assert_eq!(page_of(2).first_segment_index(MAX_SEGMENTS_PER_PAGE), 4);
    }

    /// Page 0 would collide with segment 0 of page 1, so it is rejected rather
    /// than quietly treated as the first page.
    #[test]
    fn segment_page_zero_is_rejected() {
        let mut frame = vec![0xaa, 0xa5, 0x00];
        frame.resize(19, 0);
        frame.push(calculate_checksum(&frame));

        assert!(matches!(
            MGR.decode_for_sku(GENERIC_LIGHT, &frame),
            GoveeBlePacket::Generic(_)
        ));
    }
}
