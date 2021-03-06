extern crate bit_field;
extern crate byteorder;
extern crate emu;
extern crate slog;
use self::bit_field::BitField;
use self::byteorder::{BigEndian, LittleEndian};
use self::emu::bus::Device;
use super::super::r4300::R4300;
use super::pipeline::PixelPipeline;
use super::raster::{draw_rect, fill_rect, fill_rect_pp, DpRenderState};
use super::{CycleMode, DpColorFormat};
use emu::fp::formats::*;
use emu::fp::Q;
use emu::gfx::*;
use emu::int::Numerics;
use std::marker::PhantomData;

#[derive(Copy, Clone, Default, Debug)]
struct TileDescriptor {
    color_format: DpColorFormat,
    bpp: usize,
    pitch: usize,
    tmem_addr: u32,
    palette: usize,
    clamp: [bool; 2],
    mirror: [bool; 2],
    mask: [u32; 2],
    shift: [u32; 2],

    rect: Rect<U30F2>,
}

#[derive(Copy, Clone, Default, Debug)]
struct ImageFormat {
    color_format: DpColorFormat,
    bpp: usize,
    width: usize,
    dram_addr: u32,
}

impl ImageFormat {
    fn pitch(&self) -> usize {
        self.width * self.bpp / 8
    }
}

pub struct Rdp {
    logger: slog::Logger,
    tmem: Box<[u8]>,
    clip: Rect<I30F2>,
    fb: ImageFormat,
    tex: ImageFormat,
    tiles: [TileDescriptor; 8],
    fill_color: u32,
    cycle_mode: CycleMode,

    pipeline: PixelPipeline,

    cmdbuf: [u64; 16],
    cmdlen: usize,
}

impl Rdp {
    pub fn new(logger: slog::Logger) -> Rdp {
        let mut tmem = Vec::new();
        tmem.resize(4096, 0);
        Rdp {
            logger: logger,
            tmem: tmem.into_boxed_slice(),
            clip: Rect::default(),
            fb: ImageFormat::default(),
            tex: ImageFormat::default(),
            tiles: [TileDescriptor::default(); 8],
            fill_color: 0,
            cycle_mode: CycleMode::One,
            pipeline: PixelPipeline::new(),
            cmdbuf: [0u64; 16],
            cmdlen: 0,
        }
    }

    fn parse_color_format(&self, bits: u64) -> DpColorFormat {
        DpColorFormat::from_bits(bits as usize)
            .or_else(|| {
                error!(self.logger, "invalid color format"; "format" => bits);
                Some(DpColorFormat::Rgba)
            })
            .unwrap()
    }

    fn framebuffer<'s, 'r: 's>(&'s self) -> (&'r mut [u8], usize, usize, usize) {
        let fb_mem = R4300::get_mut()
            .bus
            .fetch_write::<u8>(self.fb.dram_addr)
            .mem()
            .unwrap();
        (fb_mem, 320, 240, self.fb.pitch())
    }

    pub fn op(&mut self, cmd: u64) {
        info!(self.logger, "DP command"; "cmd" => cmd.hex());
        self.cmdbuf[self.cmdlen] = cmd;
        self.cmdlen += 1;

        let op = self.cmdbuf[0].get_bits(56..62);
        match op {
            0x2D => {
                // Set Scissor
                self.clip = Rect::from_bits(
                    cmd.get_bits(44..56) as i32,
                    cmd.get_bits(32..44) as i32,
                    cmd.get_bits(12..24) as i32,
                    cmd.get_bits(0..12) as i32,
                );
                info!(self.logger, "DP: Set Scissor"; "clip" => ?self.clip);
                self.cmdlen = 0;
            }
            0x3D | 0x3F => {
                // Set Color/Texture Image
                let format = ImageFormat {
                    color_format: self.parse_color_format(cmd.get_bits(53..56)),
                    bpp: 4 << cmd.get_bits(51..53),
                    width: cmd.get_bits(32..42) as usize + 1,
                    dram_addr: cmd.get_bits(0..26) as u32,
                };

                if op == 0x3F {
                    self.fb = format;
                    info!(self.logger, "DP: Set Color Image"; "format" => ?self.fb);
                } else {
                    self.tex = format;
                    info!(self.logger, "DP: Set Texture Image"; "format" => ?self.tex);
                }
                self.cmdlen = 0;
            }
            0x28 => {
                // Sync Tile
                info!(self.logger, "DP: Sync Tile");
                self.cmdlen = 0;
            }
            0x2F => {
                // Set Other Modes
                self.cycle_mode = match cmd.get_bits(52..54) {
                    0 => CycleMode::One,
                    1 => CycleMode::Two,
                    2 => CycleMode::Copy,
                    3 => CycleMode::Fill,
                    _ => unreachable!(),
                };
                self.pipeline.set_other_modes(cmd);
                warn!(self.logger, "DP: Set Other Modes"; "blender" => self.pipeline.fmt_blender());
                self.cmdlen = 0;
            }
            0x24 => {
                // Texture rectangle (2 words)
                if self.cmdlen != 2 {
                    return;
                }

                let tile = self.cmdbuf[0].get_bits(24..27) as usize;
                let x1 = self.cmdbuf[0].get_bits(44..56) as u32;
                let y1 = self.cmdbuf[0].get_bits(32..44) as u32;
                let x0 = self.cmdbuf[0].get_bits(12..24) as u32;
                let y0 = self.cmdbuf[0].get_bits(0..12) as u32;
                let mut rect = Rect::<U30F2>::from_bits(x0, y0, x1, y1);

                let s = Q::<I6F10>::from_bits(self.cmdbuf[1].get_bits(48..64) as i16);
                let t = Q::<I6F10>::from_bits(self.cmdbuf[1].get_bits(32..48) as i16);
                let dsdx = Q::<I6F10>::from_bits(self.cmdbuf[1].get_bits(16..32) as i16);
                let dtdy = Q::<I6F10>::from_bits(self.cmdbuf[1].get_bits(0..16) as i16);

                let ptex = Point::new(s, t);
                let slope = Point::new(dsdx, dtdy);
                info!(self.logger, "DP: Textured Rectangle"; "idx" => tile, "tile" => ?self.tiles[tile], "screen" => ?rect, "ptex" => ?ptex, "slope" => ?slope);

                let tmem_addr = self.tiles[tile].tmem_addr as usize;
                let tmem_pitch = self.tiles[tile].pitch;
                let tex_rect = self.tiles[tile].rect;
                let src = (
                    &self.tmem[tmem_addr..],
                    tex_rect.width().floor() as usize + 1,
                    tex_rect.height().floor() as usize + 1,
                    tmem_pitch,
                );

                let mut fb_writer = R4300::get_mut().bus.fetch_write::<u8>(self.fb.dram_addr);
                let fb_mem = fb_writer.mem().unwrap();
                let dst = (fb_mem, 320, 240, self.fb.pitch());

                // FIXME: draw_rect_slopes() use inclusive rectangles... maybe we need clipping?
                let w = rect.width() - 1;
                let h = rect.height() - 1;
                rect.set_width(w);
                rect.set_height(h);

                let state = DpRenderState {
                    dst_cf: self.fb.color_format,
                    dst_bpp: self.fb.bpp,
                    src_cf: self.tiles[tile].color_format,
                    src_bpp: self.tiles[tile].bpp,
                    phantom: PhantomData,
                };
                state.draw_rect_slopes(dst, rect, src, ptex.cast(), slope.cast());

                self.cmdlen = 0;
            }
            0x34 => {
                // Load Tile
                let tile = cmd.get_bits(24..27) as usize;
                let s0 = cmd.get_bits(44..56) as u32;
                let t0 = cmd.get_bits(32..44) as u32;
                let s1 = cmd.get_bits(12..24) as u32;
                let t1 = cmd.get_bits(0..12) as u32;
                let mut rect = Rect::<U30F2>::from_bits(s0, t0, s1, t1);
                info!(self.logger, "DP: Load Tile"; "idx" => tile, "rect" => ?rect);

                // Load_Tile also updates the internal tile rect
                self.tiles[tile].rect = rect;

                let tmem_addr = self.tiles[tile].tmem_addr as usize;
                let tmem_pitch = self.tiles[tile].pitch;
                let tex_reader = R4300::get().bus.fetch_read::<u8>(self.tex.dram_addr);
                let tex_mem = tex_reader.mem().unwrap();
                let width = rect.width().floor() as usize + 1;
                let height = rect.height().floor() as usize + 1;

                let copy_width = width.min(self.tex.width); // FIXME: is this correct? See RDPI4Decode
                rect.set_width(Q::from_int(copy_width as u32 - 1));

                info!(self.logger, "DP: Load Tile: draw_rect"; "rect" => ?rect, "copy_width" => copy_width);
                if self.tiles[tile].bpp == 16 && self.tex.bpp == 16 {
                    let mut tmem = GfxBufferMutLE::<Rgba5551>::new(
                        &mut self.tmem[tmem_addr..],
                        copy_width,
                        height,
                        tmem_pitch,
                    )
                    .unwrap();

                    let tex = GfxBufferLE::<Rgba5551>::new(
                        &tex_mem,
                        copy_width,
                        height,
                        self.tex.pitch(),
                    )
                    .unwrap();

                    draw_rect(
                        &mut tmem,
                        Point::<U30F2>::from_int(0, 0),
                        &tex,
                        rect.cast::<U27F5>(),
                    );
                } else if self.tiles[tile].bpp == 8 && self.tex.bpp == 8 {
                    let mut tmem = GfxBufferMutLE::<I8>::new(
                        &mut self.tmem[tmem_addr..],
                        copy_width,
                        height,
                        tmem_pitch,
                    )
                    .unwrap();

                    let tex =
                        GfxBufferLE::<I8>::new(&tex_mem, copy_width, height, self.tex.pitch())
                            .unwrap();

                    draw_rect(
                        &mut tmem,
                        Point::<U30F2>::from_int(0, 0),
                        &tex,
                        rect.cast::<U27F5>(),
                    );
                } else {
                    panic!(
                        "unknown src/dst bpp combination in load tile: dst={} src={}",
                        self.tiles[tile].bpp, self.tex.bpp,
                    );
                }

                self.cmdlen = 0;
            }
            0x35 => {
                // Set Tile
                let idx = cmd.get_bits(24..27) as usize;
                let color_format = self.parse_color_format(cmd.get_bits(53..56));
                let tile = &mut self.tiles[idx];
                tile.color_format = color_format;
                tile.bpp = 4 << cmd.get_bits(51..53);
                tile.pitch = cmd.get_bits(41..50) as usize * 8;
                tile.tmem_addr = cmd.get_bits(32..41) as u32 * 8;
                tile.palette = cmd.get_bits(20..24) as usize;
                tile.clamp[0] = cmd.get_bit(9);
                tile.clamp[1] = cmd.get_bit(19);
                tile.mirror[0] = cmd.get_bit(8);
                tile.mirror[1] = cmd.get_bit(18);
                tile.mask[0] = (1 << cmd.get_bits(4..8)) - 1;
                tile.mask[1] = (1 << cmd.get_bits(14..18)) - 1;
                tile.shift[0] = cmd.get_bits(0..4) as u32;
                tile.shift[1] = cmd.get_bits(10..14) as u32;
                info!(self.logger, "DP: Set Tile"; "idx" => idx, "format" => ?tile);
                self.cmdlen = 0;
            }
            0x36 => {
                let x1 = cmd.get_bits(44..56) as u32;
                let y1 = cmd.get_bits(32..44) as u32;
                let x0 = cmd.get_bits(12..24) as u32;
                let y0 = cmd.get_bits(0..12) as u32;
                let mut rect = Rect::<U30F2>::from_bits(x0, y0, x1, y1);
                info!(self.logger, "DP: Fill Rectangle"; "rect" => ?rect);

                match self.cycle_mode {
                    CycleMode::Fill => {
                        // Fill rectangle works with 32-bit packed words. Thus, we treat everything
                        // as RGBA8888, but we need to convert the rect coordinates to adjust them
                        // to a fake 32-bit resolution.
                        let bppconv = 32 / self.fb.bpp as u32;

                        rect.c0.x /= bppconv;
                        rect.c0.y /= bppconv;
                        rect.c1.x = ((rect.c1.x + 1) / bppconv) - 1;
                        rect.c1.y = ((rect.c1.y + 1) / bppconv) - 1;

                        if rect.truncate().cast::<U30F2>() != rect {
                            panic!("Coordinates in DP Fill Rectangle were not 32-bit aligned");
                        }

                        let fb = self.framebuffer();
                        let mut dst = GfxBufferMut::<Rgba8888, BigEndian>::new(
                            fb.0,
                            fb.1 / bppconv as usize,
                            fb.2,
                            fb.3,
                        )
                        .unwrap();
                        let color = Color::<Rgba8888>::from_bits(self.fill_color);
                        fill_rect(&mut dst, rect, color);
                    }
                    CycleMode::One => {
                        let fb = self.framebuffer();
                        let mut dst =
                            GfxBufferMut::<Rgba8888, LittleEndian>::new(fb.0, fb.1, fb.2, fb.3)
                                .unwrap();

                        if rect.truncate().cast::<U30F2>() != rect {
                            panic!("Coordinates in DP Fill Rectangle were not 32-bit aligned");
                        }

                        let color = Color::<Abgr8888>::from_bits(self.fill_color); // FIXME: this is probably not correct
                        fill_rect_pp(&mut dst, rect, color, &mut self.pipeline);
                    }
                    _ => unimplemented!(),
                }
                self.cmdlen = 0;
            }
            0x37 => {
                let color = cmd.get_bits(0..32) as u32;
                info!(self.logger, "DP: Set Fill Color"; "color" => color.hex());
                self.fill_color = color;
                self.cmdlen = 0;
            }
            0x3C => {
                // Set Combine Mode
                self.pipeline.set_combine_mode(cmd);
                info!(self.logger, "DP: Set Combine Mode"; "cmd" => cmd.hex(), "cc" => self.pipeline.fmt_combiner());
                self.cmdlen = 0;
            }
            0x39 => {
                // Set Blend Color
                let c = Color::<Abgr8888>::from_bits(cmd as u32);
                self.pipeline.set_blend_color(c.cconv());
                info!(self.logger, "DP: Set Blend Color"; "c" => ?c);
                self.cmdlen = 0;
            }

            _ => {
                warn!(self.logger, "unimplemented command"; "cmd" => (((cmd>>56)&0x3F) as u8).hex());
                self.cmdlen = 0;
            }
        };
    }
}
