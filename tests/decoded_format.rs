//! The SVQ1 and SVQ3 registry decoders report the size of the frame they
//! last returned (oxideav-core `Decoder::output_video_dimensions` /
//! `output_pixel_format`).
//!
//! SVQ1 carries its size in every intra frame header, so a new intra
//! frame may change it; the streams come from the crate's own encoder.
//! SVQ3 takes its size from the `SEQH` in extradata, fixed for the
//! stream; its pictures are coded in whole macroblocks and cropped to it.
//! The SVQ3 cases read the FFmpeg FATE samples at `$FATE_SUITE/svq3`
//! through `ffprobe` and are skipped when either is missing.

#![cfg(feature = "registry")]

use oxideav_core::{CodecId, CodecParameters, Frame, Packet, PixelFormat, TimeBase, VideoFrame};

fn packet(data: Vec<u8>) -> Packet {
    Packet::new(0, TimeBase::new(1, 25), data)
}

/// Asserts that `frame` holds a 4:2:0 picture of `w`×`h` luma samples.
fn assert_geometry(frame: &VideoFrame, (w, h): (u32, u32), at: &str) {
    let planes = frame.image_planes();
    assert_eq!(planes.len(), 3, "{at}: planes");
    for (i, plane) in planes.iter().enumerate() {
        let (pw, ph) = if i == 0 {
            (w as usize, h as usize)
        } else {
            (w.div_ceil(2) as usize, h.div_ceil(2) as usize)
        };
        assert!(plane.stride >= pw, "{at}: plane {i} stride");
        assert!(
            plane.data.len() >= plane.stride * (ph - 1) + pw,
            "{at}: plane {i} rows"
        );
    }
}

/// `frames` SVQ1 packets of a `w`×`h` testcard from the registry encoder
/// (an intra frame first).
fn svq1_stream(w: u32, h: u32, frames: usize) -> Vec<Vec<u8>> {
    let mut params = CodecParameters::video(CodecId::new("svq1"));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    let mut enc = oxideav_svq::make_encoder(&params).expect("encoder");
    let plane = |pw: u32, ph: u32, seed: usize| oxideav_core::VideoPlane {
        stride: pw as usize,
        data: (0..(pw * ph) as usize)
            .map(|i| ((i * 7 + seed * 13) % 256) as u8)
            .collect(),
    };
    let mut out = Vec::new();
    for n in 0..frames {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let frame = VideoFrame {
            pts: Some(n as i64),
            planes: vec![plane(w, h, n), plane(cw, ch, n + 1), plane(cw, ch, n + 2)],
        };
        enc.send_frame(&Frame::Video(frame)).expect("encode");
        while let Ok(p) = enc.receive_packet() {
            out.push(p.data);
        }
    }
    enc.flush().expect("flush");
    while let Ok(p) = enc.receive_packet() {
        out.push(p.data);
    }
    assert_eq!(out.len(), frames, "{w}x{h}: packets");
    out
}

/// An odd-size intra + inter pair, then an intra frame of another size.
#[test]
fn svq1_reports_each_frames_size() {
    let packets: Vec<(Vec<u8>, (u32, u32))> = svq1_stream(33, 17, 2)
        .into_iter()
        .map(|p| (p, (33, 17)))
        .chain(svq1_stream(48, 32, 1).into_iter().map(|p| (p, (48, 32))))
        .collect();
    let mut dec =
        oxideav_svq::make_decoder(&CodecParameters::video(CodecId::new("svq1"))).expect("decoder");
    let mut last = None;
    for (at, (data, size)) in packets.into_iter().enumerate() {
        dec.send_packet(&packet(data)).expect("send");
        if last.is_some() {
            assert_eq!(
                dec.output_video_dimensions(),
                last,
                "frame {at}: before it is returned"
            );
        }
        let frame = match dec.receive_frame() {
            Ok(Frame::Video(frame)) => frame,
            other => panic!("frame {at}: {other:?}"),
        };
        assert_eq!(dec.output_video_dimensions(), Some(size), "frame {at}");
        assert_eq!(
            dec.output_pixel_format(),
            Some(PixelFormat::Yuv420P),
            "frame {at}"
        );
        assert_geometry(&frame, size, &format!("frame {at}"));
        last = Some(size);
    }
}

/// The bytes of an `ffprobe -show_data` hex dump that starts after line
/// `start` of `lines`.
fn hex_dump(lines: &[&str], start: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for line in &lines[start + 1..] {
        let Some(hex) = line.get(10..).filter(|_| line.get(8..10) == Some(": ")) else {
            break;
        };
        let hex = hex.split("  ").next().unwrap_or("");
        for group in hex.split(' ') {
            for k in (0..group.len()).step_by(2) {
                out.push(u8::from_str_radix(&group[k..k + 2], 16).unwrap());
            }
        }
    }
    out
}

/// The SVQ3 extradata and first two packets of a FATE sample, or `None`
/// when the sample or `ffprobe` is missing.
fn svq3_sample(name: &str) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let path = std::path::Path::new(&std::env::var_os("FATE_SUITE")?)
        .join("svq3")
        .join(name);
    if !path.is_file() {
        return None;
    }
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_streams",
            "-show_packets",
        ])
        .args(["-show_data", "-read_intervals", "%+#2", "-of", "default"])
        .arg(&path)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let mut extradata = None;
    let mut packets = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match *line {
            "extradata=" => extradata = Some(hex_dump(&lines, i)),
            "data=" => packets.push(hex_dump(&lines, i)),
            _ => {}
        }
    }
    Some((extradata?, packets))
}

#[test]
fn svq3_reports_the_cropped_sequence_size() {
    // (sample, SEQH size, frames to check): 320×240 is whole macroblocks;
    // 480×257 is coded at 480×272 and cropped. The second picture of the
    // 480×257 sample is a B slice, which this decoder does not decode.
    let cases = [
        ("Vertical400kbit.sorenson3.mov", (320, 240), 2),
        ("svq3_decoding_regression.mov", (480, 257), 1),
    ];
    for (name, size, frames) in cases {
        let Some((extradata, packets)) = svq3_sample(name) else {
            eprintln!("skip {name}: set FATE_SUITE and install ffprobe");
            continue;
        };
        assert_eq!(packets.len(), 2, "{name}: packets");
        let mut params = CodecParameters::video(CodecId::new("svq3"));
        params.extradata = extradata;
        let mut dec = oxideav_svq::make_svq3_decoder(&params).expect("decoder");
        for (at, data) in packets.into_iter().take(frames).enumerate() {
            dec.send_packet(&packet(data)).expect("send");
            let frame = match dec.receive_frame() {
                Ok(Frame::Video(frame)) => frame,
                other => panic!("{name} frame {at}: {other:?}"),
            };
            assert_eq!(
                dec.output_video_dimensions(),
                Some(size),
                "{name} frame {at}"
            );
            assert_eq!(
                dec.output_pixel_format(),
                Some(PixelFormat::Yuv420P),
                "{name} frame {at}"
            );
            assert_geometry(&frame, size, &format!("{name} frame {at}"));
            // Cropped, not macroblock-padded.
            assert_eq!(
                frame.image_planes()[0].data.len(),
                frame.image_planes()[0].stride * size.1 as usize,
                "{name} frame {at}: luma rows"
            );
        }
    }
}
