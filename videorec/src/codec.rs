//! Minimal H.264 / H.265 bitstream helpers: NAL classification and SPS parsing.
//! Only what is needed to validate frames and build sample descriptions;
//! nothing here decodes pictures.
//!
//! References: ITU-T H.264 (7.3.1 NAL unit syntax, 7.3.2.1.1 SPS syntax),
//! ITU-T H.265 (7.3.1.2 NAL unit header, 7.3.2.2 SPS syntax, 7.3.3 profile_tier_level).

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        }
    }
}

/// Removes emulation-prevention bytes (00 00 03 -> 00 00).
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    pub fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        if self.pos > self.data.len() * 8 {
            None
        } else {
            Some(())
        }
    }
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + rest as u64) as u32)
    }
    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 {
            ((k + 1) / 2) as i32
        } else {
            (-(k / 2)) as i32
        })
    }
}

/// Information about one NAL unit needed for stream assembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalInfo {
    pub nal_type: u8,
    pub is_vcl: bool,
    pub is_keyframe: bool,
    /// True if this VCL NAL starts a new picture (first slice of a frame).
    pub first_slice: bool,
    pub is_param_set: bool,
}

/// Checks the NAL header and returns classification, or None if the header is
/// not plausible for the codec. `nal` starts at the NAL header byte.
pub fn classify(codec: Codec, nal: &[u8]) -> Option<NalInfo> {
    match codec {
        Codec::H264 => {
            let h = *nal.first()?;
            if h & 0x80 != 0 {
                return None; // forbidden_zero_bit
            }
            let ref_idc = (h >> 5) & 3;
            let t = h & 0x1F;
            // Types 1..=12 are the ones a recorder realistically emits; 13..=23
            // are reserved or extension types, rejected to cut false positives.
            if t == 0 || t > 12 {
                return None;
            }
            // SPS/PPS/IDR must have nal_ref_idc != 0; SEI/AUD/end must have 0.
            match t {
                5 | 7 | 8 if ref_idc == 0 => return None,
                6 | 9 | 10 | 11 | 12 if ref_idc != 0 => return None,
                _ => {}
            }
            let is_vcl = (1..=5).contains(&t);
            let first_slice = is_vcl && nal.len() > 1 && (nal[1] & 0x80) != 0; // first_mb_in_slice == 0
            Some(NalInfo {
                nal_type: t,
                is_vcl,
                is_keyframe: t == 5,
                first_slice,
                is_param_set: t == 7 || t == 8,
            })
        }
        Codec::H265 => {
            if nal.len() < 2 {
                return None;
            }
            let h0 = nal[0];
            let h1 = nal[1];
            if h0 & 0x80 != 0 {
                return None;
            }
            let t = (h0 >> 1) & 0x3F;
            let layer_id = ((h0 & 1) << 5) | (h1 >> 3);
            let tid_plus1 = h1 & 7;
            if layer_id != 0 || tid_plus1 == 0 {
                return None;
            }
            // VCL 0..=9 and 16..=21 are defined; 22..=31 reserved. Non-VCL 32..=40 defined.
            let valid = (0..=9).contains(&t) || (16..=21).contains(&t) || (32..=40).contains(&t);
            if !valid {
                return None;
            }
            let is_vcl = t < 32;
            let first_slice = is_vcl && nal.len() > 2 && (nal[2] & 0x80) != 0;
            Some(NalInfo {
                nal_type: t,
                is_vcl,
                is_keyframe: (16..=21).contains(&t),
                first_slice,
                is_param_set: (32..=34).contains(&t),
            })
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SpsInfo {
    pub width: u32,
    pub height: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    /// H.264 only: needed to read frame_num from slice headers.
    pub log2_max_frame_num: u32,
    pub separate_colour_plane: bool,
    /// Bits of pic_order_cnt_lsb (H.264 with poc_type 0, H.265).
    pub log2_max_poc_lsb: u32,
    /// H.264 only.
    pub poc_type: u32,
    pub delta_pic_order_always_zero: bool,
    pub frame_mbs_only: bool,
    /// H.265 only: the 12 profile_tier_level bytes used in hvcC.
    #[serde(skip)]
    pub hevc_ptl: [u8; 12],
}

fn plausible(s: &SpsInfo) -> bool {
    (16..=8192).contains(&s.width)
        && (16..=8192).contains(&s.height)
        && s.chroma_format_idc <= 3
        && (8..=16).contains(&s.bit_depth_luma)
}

/// Parses an SPS NAL (including the NAL header). Returns None if the values
/// are not plausible — used as a strong false-positive filter when carving.
pub fn parse_sps(codec: Codec, nal: &[u8]) -> Option<SpsInfo> {
    let s = match codec {
        Codec::H264 => parse_sps_h264(&unescape(nal.get(1..)?))?,
        Codec::H265 => parse_sps_h265(&unescape(nal.get(2..)?))?,
    };
    plausible(&s).then_some(s)
}

fn parse_sps_h264(r: &[u8]) -> Option<SpsInfo> {
    let mut b = BitReader::new(r);
    let profile_idc = b.bits(8)? as u8;
    b.bits(8)?; // constraint flags
    let level_idc = b.bits(8)? as u8;
    if b.ue()? > 31 {
        return None;
    }
    let mut chroma = 1;
    let (mut bdl, mut bdc) = (8, 8);
    let mut sep = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = b.ue()?;
        if chroma > 3 {
            return None;
        }
        if chroma == 3 {
            sep = b.bit()? == 1;
        }
        bdl = b.ue()? + 8;
        bdc = b.ue()? + 8;
        b.bit()?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            let n = if chroma != 3 { 8 } else { 12 };
            for i in 0..n {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            let d = b.se()?;
                            next = (last + d + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    let log2_max_frame_num = b.ue()? + 4;
    if log2_max_frame_num > 16 {
        return None;
    }
    let poc_type = b.ue()?;
    let mut log2_max_poc_lsb = 0;
    let mut dpoaz = false;
    if poc_type == 0 {
        log2_max_poc_lsb = b.ue()? + 4;
        if log2_max_poc_lsb > 16 {
            return None;
        }
    } else if poc_type == 1 {
        dpoaz = b.bit()? == 1;
        b.se()?;
        b.se()?;
        let n = b.ue()?;
        if n > 255 {
            return None;
        }
        for _ in 0..n {
            b.se()?;
        }
    } else if poc_type > 2 {
        return None;
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_units = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?; // direct_8x8_inference
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.bit()? == 1 {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let (ux, uy) = match chroma {
        0 => (1, 2 - frame_mbs_only),
        1 => (2, 2 * (2 - frame_mbs_only)),
        2 => (2, 2 - frame_mbs_only),
        _ => (1, 2 - frame_mbs_only),
    };
    let width = (w_mbs * 16).checked_sub(ux * (cl + cr))?;
    let height = ((2 - frame_mbs_only) * h_units * 16).checked_sub(uy * (ct + cb))?;
    Some(SpsInfo {
        width,
        height,
        profile_idc,
        level_idc,
        chroma_format_idc: chroma,
        bit_depth_luma: bdl,
        bit_depth_chroma: bdc,
        log2_max_frame_num,
        separate_colour_plane: sep,
        log2_max_poc_lsb,
        poc_type,
        delta_pic_order_always_zero: dpoaz,
        frame_mbs_only: frame_mbs_only == 1,
        hevc_ptl: [0; 12],
    })
}

fn parse_sps_h265(r: &[u8]) -> Option<SpsInfo> {
    let mut b = BitReader::new(r);
    b.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.bits(3)?;
    if max_sub_layers_minus1 > 6 {
        return None;
    }
    b.bit()?; // temporal_id_nesting
    // general profile_tier_level: 12 bytes, byte aligned here.
    let ptl: [u8; 12] = r.get(1..13)?.try_into().ok()?;
    b.skip(96)?;
    let profile_idc = ptl[0] & 0x1F;
    let level_idc = ptl[11];
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = b.bit()? == 1;
        sub_level[i] = b.bit()? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            b.bits(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            b.skip(88)?;
        }
        if sub_level[i] {
            b.skip(8)?;
        }
    }
    if b.ue()? > 15 {
        return None; // sps_seq_parameter_set_id
    }
    let chroma = b.ue()?;
    if chroma > 3 {
        return None;
    }
    let mut sep = false;
    if chroma == 3 {
        sep = b.bit()? == 1;
    }
    let mut width = b.ue()?;
    let mut height = b.ue()?;
    if b.bit()? == 1 {
        let (sx, sy) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let l = b.ue()?;
        let rr = b.ue()?;
        let t = b.ue()?;
        let bt = b.ue()?;
        width = width.checked_sub(sx * (l + rr))?;
        height = height.checked_sub(sy * (t + bt))?;
    }
    let bdl = b.ue()? + 8;
    let bdc = b.ue()? + 8;
    let log2_max_poc_lsb = b.ue()? + 4;
    if log2_max_poc_lsb > 16 {
        return None;
    }
    Some(SpsInfo {
        width,
        height,
        profile_idc,
        level_idc,
        chroma_format_idc: chroma,
        bit_depth_luma: bdl,
        bit_depth_chroma: bdc,
        log2_max_frame_num: 0,
        separate_colour_plane: sep,
        log2_max_poc_lsb,
        poc_type: 0,
        delta_pic_order_always_zero: false,
        frame_mbs_only: true,
        hevc_ptl: ptl,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Pps264 {
    pub cabac: bool,
    pub bottom_field_pic_order: bool,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub num_ref_idx_default: [u32; 2],
    pub pic_init_qp: i32,
    pub deblocking_control: bool,
    pub redundant_pic_cnt: bool,
}

/// Fields of a PPS needed to read slice headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct PpsInfo {
    pub id: u32,
    /// H.264 fields used to walk the slice header up to slice_qp_delta.
    pub h264: Option<Pps264>,
    pub dependent_slices: bool,
    pub output_flag_present: bool,
    pub extra_slice_header_bits: u32,
}

pub fn parse_pps(codec: Codec, nal: &[u8]) -> Option<PpsInfo> {
    let hdr = if codec == Codec::H264 { 1 } else { 2 };
    let r = unescape(nal.get(hdr..nal.len().min(hdr + 32))?);
    let mut b = BitReader::new(&r);
    let id = b.ue()?;
    let sps_id = b.ue()?;
    if id > 255 || sps_id > 31 {
        return None;
    }
    let mut p = PpsInfo {
        id,
        ..Default::default()
    };
    if codec == Codec::H264 {
        let cabac = b.bit()? == 1;
        let bottom = b.bit()? == 1;
        if b.ue()? == 0 {
            // (slice groups make the rest of the header irrelevant here)
            let l0 = b.ue()?;
            let l1 = b.ue()?;
            let weighted_pred = b.bit()? == 1;
            let weighted_bipred_idc = b.bits(2)?;
            let qp = 26 + b.se()?;
            b.se()?;
            b.se()?;
            let deblocking_control = b.bit()? == 1;
            b.bit()?;
            let redundant = b.bit()? == 1;
            if (0..=51).contains(&qp) && weighted_bipred_idc < 3 {
                p.h264 = Some(Pps264 {
                    cabac,
                    bottom_field_pic_order: bottom,
                    weighted_pred,
                    weighted_bipred_idc,
                    num_ref_idx_default: [l0, l1],
                    pic_init_qp: qp,
                    deblocking_control,
                    redundant_pic_cnt: redundant,
                });
            }
        }
    }
    if codec == Codec::H265 {
        p.dependent_slices = b.bit()? == 1;
        p.output_flag_present = b.bit()? == 1;
        p.extra_slice_header_bits = b.bits(3)?;
    }
    Some(p)
}

/// Walks an H.264 slice header from after frame_num to slice_qp_delta and the
/// deblocking parameters, checking every value against its legal range
/// (ITU-T H.264 7.3.3 / 7.4.3). Random data rarely survives this. Returns
/// true when the header is consistent or cannot be checked further
/// (explicit weighted prediction tables are not walked).
fn h264_rest_ok(b: &mut BitReader, nal: &[u8], st: u32, s: &SpsInfo, p: &Pps264) -> bool {
    let run = |b: &mut BitReader| -> Option<bool> {
        let idr = nal[0] & 0x1F == 5;
        let ref_idc = (nal[0] >> 5) & 3;
        let mut field = false;
        if !s.frame_mbs_only {
            field = b.bit()? == 1;
            if field {
                b.bit()?;
            }
        }
        if idr && b.ue()? > 65535 {
            return Some(false);
        }
        if s.poc_type == 0 {
            b.bits(s.log2_max_poc_lsb)?;
            if p.bottom_field_pic_order && !field {
                b.se()?;
            }
        }
        if s.poc_type == 1 && !s.delta_pic_order_always_zero {
            b.se()?;
            if p.bottom_field_pic_order && !field {
                b.se()?;
            }
        }
        if p.redundant_pic_cnt && b.ue()? > 127 {
            return Some(false);
        }
        let (is_p, is_b, is_i) = (st == 0 || st == 3, st == 1, st == 2 || st == 4);
        if is_b {
            b.bit()?; // direct_spatial_mv_pred_flag
        }
        let mut nref = p.num_ref_idx_default;
        if (is_p || is_b) && b.bit()? == 1 {
            nref[0] = b.ue()?;
            if is_b {
                nref[1] = b.ue()?;
            }
        }
        if nref[0] > 31 || nref[1] > 31 {
            return Some(false);
        }
        // ref_pic_list_modification
        for list in 0..(if is_b { 2 } else { 1 }) {
            let _ = list;
            if !is_i && b.bit()? == 1 {
                for _ in 0..33 {
                    match b.ue()? {
                        0..=2 => {
                            b.ue()?;
                        }
                        3 => break,
                        _ => return Some(false),
                    }
                }
            }
        }
        if (p.weighted_pred && is_p) || (p.weighted_bipred_idc == 1 && is_b) {
            // pred_weight_table (7.3.3.2)
            let chroma = !s.separate_colour_plane && s.chroma_format_idc != 0;
            if b.ue()? > 7 {
                return Some(false);
            }
            if chroma && b.ue()? > 7 {
                return Some(false);
            }
            let w = |b: &mut BitReader| -> Option<bool> {
                let x = b.se()?;
                let y = b.se()?;
                Some((-128..=127).contains(&x) && (-128..=127).contains(&y))
            };
            for list in 0..(if is_b { 2 } else { 1 }) {
                for _ in 0..=nref[list] {
                    if b.bit()? == 1 && !w(b)? {
                        return Some(false);
                    }
                    if chroma && b.bit()? == 1 && !(w(b)? && w(b)?) {
                        return Some(false);
                    }
                }
            }
        }
        if ref_idc != 0 {
            if idr {
                b.bits(2)?;
            } else if b.bit()? == 1 {
                for _ in 0..66 {
                    match b.ue()? {
                        0 => break,
                        1 | 2 | 6 => {
                            b.ue()?;
                        }
                        3 => {
                            b.ue()?;
                            b.ue()?;
                        }
                        4 => {
                            b.ue()?;
                        }
                        5 => {}
                        _ => return Some(false),
                    }
                }
            }
        }
        if p.cabac && !is_i && b.ue()? > 2 {
            return Some(false);
        }
        let qp = p.pic_init_qp + b.se()?;
        let min_qp = -6 * (s.bit_depth_luma as i32 - 8);
        if qp < min_qp || qp > 51 {
            return Some(false);
        }
        if st == 3 || st == 4 {
            return Some(true); // SP/SI fields not walked
        }
        if p.deblocking_control {
            let idc = b.ue()?;
            if idc > 2 {
                return Some(false);
            }
            if idc != 1 {
                let a = b.se()?;
                let be = b.se()?;
                if !(-6..=6).contains(&a) || !(-6..=6).contains(&be) {
                    return Some(false);
                }
            }
        }
        Some(true)
    };
    // Running out of the 40 bytes read is not an error.
    run(b).unwrap_or(true)
}

/// Fields read from the start of a slice header.
#[derive(Debug, Clone, Copy)]
pub struct SliceHead {
    pub pps_id: u32,
    pub slice_type: u32,
    /// H.264: frame_num (needs the SPS).
    pub frame_num: Option<u32>,
    /// H.265: slice_pic_order_cnt_lsb of the first slice (needs SPS and PPS);
    /// Some(0) for IDR pictures.
    pub poc_lsb: Option<u32>,
}

/// Parses the beginning of a slice header. Without `sps`/`pps`, only the
/// context-free fields are checked.
pub fn slice_head(
    codec: Codec,
    nal: &[u8],
    sps: Option<&SpsInfo>,
    pps: Option<&PpsInfo>,
) -> Option<SliceHead> {
    match codec {
        Codec::H264 => {
            let r = unescape(nal.get(1..nal.len().min(64))?);
            let mut b = BitReader::new(&r);
            b.ue()?; // first_mb_in_slice
            let slice_type = b.ue()?;
            if slice_type > 9 {
                return None;
            }
            let pps_id = b.ue()?;
            if pps_id > 255 {
                return None;
            }
            let frame_num = match sps {
                Some(s) if s.log2_max_frame_num > 0 => {
                    if s.separate_colour_plane {
                        b.bits(2)?;
                    }
                    Some(b.bits(s.log2_max_frame_num)?)
                }
                _ => None,
            };
            if let (Some(s), Some(p)) = (sps, pps.and_then(|p| p.h264))
                && !h264_rest_ok(&mut b, nal, slice_type % 5, s, &p)
            {
                return None;
            }
            Some(SliceHead {
                pps_id,
                slice_type,
                frame_num,
                poc_lsb: None,
            })
        }
        Codec::H265 => {
            let t = (nal.first()? >> 1) & 0x3F;
            let r = unescape(nal.get(2..nal.len().min(40))?);
            let mut b = BitReader::new(&r);
            let first = b.bit()? == 1;
            if (16..=23).contains(&t) {
                b.bit()?; // no_output_of_prior_pics_flag
            }
            let pps_id = b.ue()?;
            if pps_id > 63 {
                return None;
            }
            let mut head = SliceHead {
                pps_id,
                slice_type: 0,
                frame_num: None,
                poc_lsb: None,
            };
            let (Some(s), Some(p)) = (sps, pps) else {
                return Some(head);
            };
            if !first {
                return Some(head); // slice_segment_address needs the CTB size
            }
            b.skip(p.extra_slice_header_bits as usize)?;
            head.slice_type = b.ue()?;
            if head.slice_type > 2 {
                return None;
            }
            if p.output_flag_present {
                b.bit()?;
            }
            if s.separate_colour_plane {
                b.bits(2)?;
            }
            head.poc_lsb = Some(if t == 19 || t == 20 {
                0
            } else {
                b.bits(s.log2_max_poc_lsb)?
            });
            Some(head)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ue_values() {
        // bits: 1 | 010 | 011 | 00100 -> 0,1,2,3
        let data = [0b1010_0110, 0b0100_0000];
        let mut b = BitReader::new(&data);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
    }

    #[test]
    fn unescape_removes_epb() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }

    #[test]
    fn h264_classify() {
        assert!(classify(Codec::H264, &[0x67, 0x64]).unwrap().is_param_set);
        assert!(classify(Codec::H264, &[0x65, 0x88]).unwrap().is_keyframe);
        assert!(classify(Codec::H264, &[0x07]).is_none()); // SPS with ref_idc 0
        assert!(classify(Codec::H264, &[0x80]).is_none());
    }
}
