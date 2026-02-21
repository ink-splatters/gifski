use std::io::BufReader;
use std::io::Read;
use imgref::ImgVec;
use gifski::Collector;
use y4m::{Colorspace, Decoder, ParseError};
use yuv::{
    yuv400_to_rgba, yuv420_to_rgba, yuv422_to_rgba, yuv444_to_rgba, YuvGrayImage, YuvPlanarImage,
    YuvRange, YuvStandardMatrix,
};
use crate::{SrcPath, BinResult};
use crate::source::{Fps, Source, DEFAULT_FPS};

pub struct Y4MDecoder {
    fps: Fps,
    in_color_space: Option<YuvStandardMatrix>,
    decoder: Decoder<Box<BufReader<dyn Read>>>,
    file_size: Option<u64>,
}

impl Y4MDecoder {
    pub fn new(src: SrcPath, fps: Fps, in_color_space: Option<YuvStandardMatrix>) -> BinResult<Self> {
        let mut file_size = None;
        let reader = match src {
            SrcPath::Path(path) => {
                let f = std::fs::File::open(path)?;
                let m = f.metadata()?;
                #[cfg(unix)] {
                    use std::os::unix::fs::MetadataExt;
                    file_size = Some(m.size());
                }
                #[cfg(windows)] {
                    use std::os::windows::fs::MetadataExt;
                    file_size = Some(m.file_size());
                }
                Box::new(BufReader::new(f)) as Box<BufReader<dyn Read>>
            },
            SrcPath::Stdin(buf) => Box::new(buf) as Box<BufReader<dyn Read>>,
        };

        Ok(Self {
            file_size,
            fps,
            in_color_space,
            decoder: Decoder::new(reader).map_err(|e| match e {
                y4m::Error::EOF => "The y4m file is truncated or invalid",
                y4m::Error::BadInput => "The y4m file contains invalid metadata",
                y4m::Error::UnknownColorspace => "y4m uses an unusual color format that is not supported",
                y4m::Error::OutOfMemory => "Out of memory, or the y4m file has bogus dimensions",
                y4m::Error::ParseError(ParseError::InvalidY4M) => "The input is not a y4m file",
                y4m::Error::ParseError(error) => return format!("y4m contains invalid data: {error}"),
                y4m::Error::IoError(error) => return format!("I/O error when reading a y4m file: {error}"),
            }.to_string())?,
        })
    }
}

enum Samp {
    Mono,
    S1x1,
    S2x1,
    S2x2,
}

impl Source for Y4MDecoder {
    fn total_frames(&self) -> Option<u64> {
        self.file_size.map(|file_size| {
            let w = self.decoder.get_width();
            let h = self.decoder.get_height();
            let d = self.decoder.get_bytes_per_sample();
            let s = match self.decoder.get_colorspace() {
                Colorspace::Cmono => 4,
                Colorspace::Cmono12 => 4,
                Colorspace::C420 => 6,
                Colorspace::C420p10 => 6,
                Colorspace::C420p12 => 6,
                Colorspace::C420jpeg => 6,
                Colorspace::C420paldv => 6,
                Colorspace::C420mpeg2 => 6,
                Colorspace::C422 => 8,
                Colorspace::C422p10 => 8,
                Colorspace::C422p12 => 8,
                Colorspace::C444 => 12,
                Colorspace::C444p10 => 12,
                Colorspace::C444p12 => 12,
                _ => 12,
            };
            file_size.saturating_sub(self.decoder.get_raw_params().len() as _) / (w * h * d * s / 4 + 6) as u64
        })
    }

    fn collect(&mut self, c: &mut Collector) -> BinResult<()> {
        let fps = self.decoder.get_framerate();
        let frame_time = 1. / (fps.num as f64 / fps.den as f64);
        let wanted_fps = f64::from(self.fps.fps.unwrap_or(DEFAULT_FPS));
        let wanted_frame_time = 1. / wanted_fps;
        let width = self.decoder.get_width();
        let height = self.decoder.get_height();
        let raw_params_str = &*String::from_utf8_lossy(self.decoder.get_raw_params()).into_owned();
        let range = raw_params_str.split_once("COLORRANGE=").map(|(_, r)| {
            if r.starts_with("FULL") { YuvRange::Full } else { YuvRange::Limited }
        });

        let matrix = self.in_color_space.unwrap_or({
            if height <= 480 && width <= 720 { YuvStandardMatrix::Bt601 } else { YuvStandardMatrix::Bt709 }
        });
        let range = range.unwrap_or(YuvRange::Limited);

        let samp = match self.decoder.get_colorspace() {
            Colorspace::Cmono => Samp::Mono,
            Colorspace::Cmono12 => return Err("Y4M with Cmono12 is not supported yet".into()),
            Colorspace::C420 => Samp::S2x2,
            Colorspace::C420p10 => return Err("Y4M with C420p10 is not supported yet".into()),
            Colorspace::C420p12 => return Err("Y4M with C420p12 is not supported yet".into()),
            Colorspace::C420jpeg => Samp::S2x2,
            Colorspace::C420paldv => Samp::S2x2,
            Colorspace::C420mpeg2 => Samp::S2x2,
            Colorspace::C422 => Samp::S2x1,
            Colorspace::C422p10 => return Err("Y4M with C422p10 is not supported yet".into()),
            Colorspace::C422p12 => return Err("Y4M with C422p12 is not supported yet".into()),
            Colorspace::C444 => Samp::S1x1,
            Colorspace::C444p10 => return Err("Y4M with C444p10 is not supported yet".into()),
            Colorspace::C444p12 => return Err("Y4M with C444p12 is not supported yet".into()),
            _ => return Err(format!("Y4M uses unsupported color mode {raw_params_str}").into()),
        };
        if width == 0 || width > u16::MAX as _ || height == 0 || height > u16::MAX as _ {
            return Err("Video too large".into());
        }

        #[cold]
        fn bad_frame(mode: &str) -> BinResult<()> {
            Err(format!("Bad Y4M frame (using {mode})").into())
        }

        let mut idx = 0;
        let mut presentation_timestamp = 0.0;
        let mut wanted_pts = 0.0;
        loop {
            match self.decoder.read_frame() {
                Ok(frame) => {
                    let this_frame_pts = presentation_timestamp / f64::from(self.fps.speed);
                    presentation_timestamp += frame_time;
                    if presentation_timestamp < wanted_pts {
                        continue; // skip a frame
                    }
                    wanted_pts += wanted_frame_time;

                    let y = frame.get_y_plane();
                    if y.is_empty() {
                        return bad_frame(raw_params_str);
                    }
                    let u = frame.get_u_plane();
                    let v = frame.get_v_plane();
                    let width_u32 = width as u32;
                    let height_u32 = height as u32;
                    let mut rgba = vec![0; width * height * 4];

                    let res = match samp {
                        Samp::Mono => {
                            let img = YuvGrayImage {
                                y_plane: y,
                                y_stride: width_u32,
                                width: width_u32,
                                height: height_u32,
                            };
                            yuv400_to_rgba(&img, &mut rgba, width_u32 * 4, range, matrix)
                        },
                        Samp::S1x1 => {
                            let img = YuvPlanarImage {
                                y_plane: y,
                                y_stride: width_u32,
                                u_plane: u,
                                u_stride: width_u32,
                                v_plane: v,
                                v_stride: width_u32,
                                width: width_u32,
                                height: height_u32,
                            };
                            yuv444_to_rgba(&img, &mut rgba, width_u32 * 4, range, matrix)
                        },
                        Samp::S2x1 => {
                            let uv_stride = width_u32.div_ceil(2);
                            let img = YuvPlanarImage {
                                y_plane: y,
                                y_stride: width_u32,
                                u_plane: u,
                                u_stride: uv_stride,
                                v_plane: v,
                                v_stride: uv_stride,
                                width: width_u32,
                                height: height_u32,
                            };
                            yuv422_to_rgba(&img, &mut rgba, width_u32 * 4, range, matrix)
                        },
                        Samp::S2x2 => {
                            let uv_stride = width_u32.div_ceil(2);
                            let img = YuvPlanarImage {
                                y_plane: y,
                                y_stride: width_u32,
                                u_plane: u,
                                u_stride: uv_stride,
                                v_plane: v,
                                v_stride: uv_stride,
                                width: width_u32,
                                height: height_u32,
                            };
                            yuv420_to_rgba(&img, &mut rgba, width_u32 * 4, range, matrix)
                        },
                    };
                    if let Err(err) = res {
                        return Err(format!("Bad Y4M frame (using {raw_params_str}): {err}").into());
                    }

                    let mut out = Vec::with_capacity(width * height);
                    for px in rgba.chunks_exact(4) {
                        out.push(rgb::RGBA8::new(px[0], px[1], px[2], px[3]));
                    }
                    if out.len() != width * height {
                        return bad_frame(raw_params_str);
                    }
                    let pixels = ImgVec::new(out, width, height);

                    c.add_frame_rgba(idx, pixels, this_frame_pts)?;
                    idx += 1;
                },
                Err(y4m::Error::EOF) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}
