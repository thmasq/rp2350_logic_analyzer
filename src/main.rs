#![no_std]
#![no_main]
#![allow(unused_crate_dependencies)]

mod dvi;
mod reset_iface;
mod usb_cli;

use core::cell::UnsafeCell;
use defmt::{debug, info};
use defmt_rtt as _;
use dvi::{ColorFormat, DviLineCommands, DviTiming};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_rp::{
    bind_interrupts,
    block::ImageDef,
    clocks::{ClockConfig, CoreVoltage, PllConfig},
    gpio::{Level, Output},
    multicore::{Stack, spawn_core1},
    peripherals::USB,
    usb::{self, Driver},
};
use embassy_time::Timer;
use embassy_usb::{
    Builder, Config as UsbConfig,
    class::cdc_acm::{CdcAcmClass, State as CdcState},
};
use portable_atomic::{AtomicBool, AtomicU32, Ordering};
use rp_pac::{
    clocks::vals::ClkHstxCtrlAuxsrc,
    dma::{
        regs::{ChTransCount, CtrlTrig},
        vals::{DataSize, TreqSel},
    },
    hstx_ctrl::regs::ExpandTmds,
};
use static_cell::StaticCell;

#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: ImageDef = ImageDef::secure_exe();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    cortex_m::peripheral::SCB::sys_reset();
}

bind_interrupts!(pub struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

pub type UsbDriver = Driver<'static, USB>;

pub static DVI_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static VIDEO_FRAME: AtomicU32 = AtomicU32::new(0);

static DVI_COMMANDS: DviLineCommands = DviLineCommands::new(&DviTiming::VGA_640X480_60HZ);

static BAD_APPLE_DATA: &[u8] = include_bytes!("bad_apple.bin");

const FB_WIDTH: usize = 80;
const FB_HEIGHT: usize = 60;
const FB_ROW_BYTES: usize = FB_WIDTH / 8; // 10 bytes per 80-pixel row
const FB_BYTES: usize = FB_ROW_BYTES * FB_HEIGHT; // 600 bytes per frame

const V_BLANK_OFF_FRONT: usize = 10;
const V_SYNC_ON: usize = 2;
const V_BLANK_TOTAL: usize = 45; // 10 + 2 + 33
const NUM_BLOCKS: usize = V_BLANK_TOTAL + (480 * 2);

struct SyncFramebuffers(UnsafeCell<[[u8; FB_BYTES]; 2]>);
unsafe impl Sync for SyncFramebuffers {}

static FB_BUFS: SyncFramebuffers = SyncFramebuffers(UnsafeCell::new([[0u8; FB_BYTES]; 2]));
static FRAME_READY: AtomicBool = AtomicBool::new(false);

/// Expand 2 source bytes (16 pixels) into 32 u32 words (128 pixels, 8x scale).
#[inline(always)]
unsafe fn expand_2_bytes_unchecked(src_row: *const u8, dst_line: *mut u32, byte_offset: usize) {
    let mut i = 0;
    while i < 2 {
        let b = unsafe { *src_row.add(byte_offset + i) };
        let base = (byte_offset + i) * 16;
        let mut bit = 0;
        while bit < 8 {
            let w = 0u32.wrapping_sub(((b >> (7 - bit)) & 1) as u32);
            let idx = base + bit * 2;
            unsafe {
                *dst_line.add(idx) = w;
                *dst_line.add(idx + 1) = w;
            }
            bit += 1;
        }
        i += 1;
    }
}

fn init_hstx() {
    // 1. Configure clk_hstx = clk_sys / 2 = 126 MHz (25.2 MHz pixel clock * 5 DDR cycles)
    let clocks = rp_pac::CLOCKS;
    clocks.clk_hstx_div().write(|w| w.set_int(2));
    clocks.clk_hstx_ctrl().write(|w| {
        w.set_auxsrc(ClkHstxCtrlAuxsrc::CLK_SYS);
        w.set_enable(true);
    });

    // 2. Reset HSTX peripheral
    let resets = rp_pac::RESETS;
    resets.reset().modify(|w| w.set_hstx(true));
    resets.reset().modify(|w| w.set_hstx(false));
    while !resets.reset_done().read().hstx() {}

    // 3. Configure GP12..GP19 for HSTX (function 0) and release RP2350 pad isolation
    for pin in 12..=19 {
        rp_pac::IO_BANK0
            .gpio(pin)
            .ctrl()
            .write(|w| w.set_funcsel(0));
        rp_pac::PADS_BANK0.gpio(pin).modify(|w| {
            w.set_iso(false);
            w.set_od(false);
        });
    }

    let hstx = rp_pac::HSTX_CTRL;

    // 4. Configure expander shifter: encoded = 4x 8-bit pixels, raw = 1x 32-bit word
    hstx.expand_shift().write(|w| {
        w.set_enc_shift(8);
        w.set_enc_n_shifts(4);
        w.set_raw_shift(0);
        w.set_raw_n_shifts(1);
    });

    // 5. Configure TMDS lane rotations and bit counts
    let (l0, l1, l2) = ColorFormat::Luma8.tmds_lanes_config();
    let tmds_val = ((l0.0 as u32) & 0x1f)
        | (((l0.1 as u32) & 0x07) << 5)
        | (((l1.0 as u32) & 0x1f) << 8)
        | (((l1.1 as u32) & 0x07) << 13)
        | (((l2.0 as u32) & 0x1f) << 16)
        | (((l2.1 as u32) & 0x07) << 21);
    hstx.expand_tmds().write_value(ExpandTmds(tmds_val));

    // 6. Configure Pico DVI Sock pin mapping:
    // bit0/1 = D0 (lane 0), bit2/3 = CLK, bit4/5 = D2 (lane 2), bit6/7 = D1 (lane 1)
    let shift_bit = |sel_p: u8, sel_n: u8, inv: bool| {
        let mut b = rp_pac::hstx_ctrl::regs::Bit(0);
        b.set_sel_p(sel_p);
        b.set_sel_n(sel_n);
        b.set_inv(inv);
        b
    };
    let clk_bit = |inv: bool| {
        let mut b = rp_pac::hstx_ctrl::regs::Bit(0);
        b.set_clk(true);
        b.set_inv(inv);
        b
    };

    hstx.bit(0).write_value(shift_bit(0, 1, false));
    hstx.bit(1).write_value(shift_bit(0, 1, true));
    hstx.bit(2).write_value(clk_bit(false));
    hstx.bit(3).write_value(clk_bit(true));
    hstx.bit(4).write_value(shift_bit(20, 21, false));
    hstx.bit(5).write_value(shift_bit(20, 21, true));
    hstx.bit(6).write_value(shift_bit(10, 11, false));
    hstx.bit(7).write_value(shift_bit(10, 11, true));

    // 7. Enable HSTX & Command Expander (clkdiv=5, clkphase=0, shift=2, n_shifts=5)
    hstx.csr().write(|w| {
        w.set_clkdiv(5);
        w.set_clkphase(0);
        w.set_shift(2);
        w.set_n_shifts(5);
        w.set_expand_en(true);
        w.set_en(true);
    });
}

fn make_dma_ctrl(chain_to: u8) -> CtrlTrig {
    let mut c = CtrlTrig(0);
    c.set_en(true);
    c.set_data_size(DataSize::SIZE_WORD);
    c.set_incr_read(true);
    c.set_incr_write(false);
    c.set_treq_sel(TreqSel::HSTX);
    c.set_chain_to(chain_to);
    c.set_irq_quiet(true);
    c
}

/// Runs entirely out of SRAM (`.data.core1_loop`) with zero calls to Flash
/// and zero Flash reads, so Core 0 XIP activity can never stall DVI timing.
#[inline(never)]
#[unsafe(link_section = ".data.core1_loop")]
fn core1_dvi_ram_loop(cmds: &DviLineCommands, line_bufs: &mut [[u32; 160]; 2]) -> ! {
    let dma = rp_pac::DMA;
    let mut front_fb: usize = 0;
    let mut cur_ch: usize = 0;
    let mut idx: usize = 2;

    loop {
        while dma.ch(cur_ch).ctrl_trig().read().busy() {
            core::hint::spin_loop();
        }

        let (ptr, len): (*const u32, u32) = if idx < V_BLANK_OFF_FRONT {
            (cmds.vblank_vsync_off.as_ptr(), 7)
        } else if idx < V_BLANK_OFF_FRONT + V_SYNC_ON {
            (cmds.vblank_vsync_on.as_ptr(), 7)
        } else if idx < V_BLANK_TOTAL {
            (cmds.vblank_vsync_off.as_ptr(), 7)
        } else {
            let active_idx = idx - V_BLANK_TOTAL;
            let y = active_idx >> 1;
            let fb_y = y >> 3;
            let buf_slot = fb_y & 1;

            if (active_idx & 1) == 0 {
                (cmds.vactive.as_ptr(), 7)
            } else {
                (unsafe { line_bufs.get_unchecked(buf_slot).as_ptr() }, 160)
            }
        };

        dma.ch(cur_ch).read_addr().write_value(ptr as u32);
        dma.ch(cur_ch).trans_count().write_value(ChTransCount(len));

        if idx >= 38 && idx < 43 {
            let fb_ptr = unsafe { (*FB_BUFS.0.get()).get_unchecked(front_fb).as_ptr() };
            let line0_ptr = unsafe { line_bufs.get_unchecked_mut(0).as_mut_ptr() };
            unsafe { expand_2_bytes_unchecked(fb_ptr, line0_ptr, (idx - 38) * 2) };
        } else if idx >= V_BLANK_TOTAL {
            let active_idx = idx - V_BLANK_TOTAL;
            if (active_idx & 1) == 1 {
                let y = active_idx >> 1;
                let sub_y = y & 7;
                let next_fb_y = (y >> 3) + 1;
                if sub_y < 5 && next_fb_y < FB_HEIGHT {
                    let fb_ptr = unsafe { (*FB_BUFS.0.get()).get_unchecked(front_fb).as_ptr() };
                    let row_ptr = unsafe { fb_ptr.add(next_fb_y * FB_ROW_BYTES) };
                    let next_slot = next_fb_y & 1;
                    let dst_ptr = unsafe { line_bufs.get_unchecked_mut(next_slot).as_mut_ptr() };
                    unsafe { expand_2_bytes_unchecked(row_ptr, dst_ptr, sub_y * 2) };
                }
            }
        }

        cur_ch ^= 1;
        idx += 1;
        if idx == NUM_BLOCKS {
            idx = 0;
            let dvi_frame = DVI_FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
            if (dvi_frame & 1) == 0 && FRAME_READY.load(Ordering::Acquire) {
                front_fb ^= 1;
                FRAME_READY.store(false, Ordering::Release);
            }
        }
    }
}

fn core1_dvi_loop() -> ! {
    static LINE_BUFS_CELL: StaticCell<[[u32; 160]; 2]> = StaticCell::new();
    static CMD_CELL: StaticCell<DviLineCommands> = StaticCell::new();

    let line_bufs = LINE_BUFS_CELL.init_with(|| [[0u32; 160]; 2]);
    let cmds = CMD_CELL.init_with(|| DVI_COMMANDS);

    // Pre-expand row 0 of frame 0 before starting DMA
    {
        let fb_ptr = unsafe { (*FB_BUFS.0.get())[0].as_ptr() };
        let line0_ptr = line_bufs[0].as_mut_ptr();
        for chunk in 0..5 {
            unsafe { expand_2_bytes_unchecked(fb_ptr, line0_ptr, chunk * 2) };
        }
    }

    let dma = rp_pac::DMA;
    let fifo_addr = rp_pac::HSTX_FIFO.fifo().as_ptr() as u32;

    let ctrl_ch0 = make_dma_ctrl(1);
    let ctrl_ch1 = make_dma_ctrl(0);

    let b0 = &cmds.vblank_vsync_off;
    dma.ch(0).read_addr().write_value(b0.as_ptr() as u32);
    dma.ch(0).write_addr().write_value(fifo_addr);
    dma.ch(0)
        .trans_count()
        .write_value(ChTransCount(b0.len() as u32));

    let b1 = &cmds.vblank_vsync_off;
    dma.ch(1).read_addr().write_value(b1.as_ptr() as u32);
    dma.ch(1).write_addr().write_value(fifo_addr);
    dma.ch(1)
        .trans_count()
        .write_value(ChTransCount(b1.len() as u32));
    dma.ch(1).al1_ctrl().write_value(ctrl_ch1.0);

    // Kick off ping-pong chain by writing ch0's trigger register
    dma.ch(0).ctrl_trig().write_value(ctrl_ch0);

    // Indirect call so ARM Thumb-2 jumps cleanly from Flash (.text) to SRAM (.data)
    let ram_loop: fn(&DviLineCommands, &mut [[u32; 160]; 2]) -> ! =
        core::hint::black_box(core1_dvi_ram_loop);
    ram_loop(cmds, line_bufs)
}

async fn frame_prefetch_task() -> ! {
    let total_vid_frames = (BAD_APPLE_DATA.len() / FB_BYTES).max(1);
    let mut next_vid_frame: usize = 1 % total_vid_frames;
    let mut back_fb: usize = 0;

    loop {
        if !FRAME_READY.load(Ordering::Acquire) {
            next_vid_frame += 1;
            if next_vid_frame >= total_vid_frames {
                next_vid_frame = 0;
            }
            let src_start = next_vid_frame * FB_BYTES;
            if src_start + FB_BYTES <= BAD_APPLE_DATA.len() {
                unsafe {
                    (&mut *FB_BUFS.0.get())[back_fb]
                        .copy_from_slice(&BAD_APPLE_DATA[src_start..src_start + FB_BYTES]);
                }
            }
            VIDEO_FRAME.store(next_vid_frame as u32, Ordering::Relaxed);
            back_fb ^= 1;
            FRAME_READY.store(true, Ordering::Release);
        } else {
            Timer::after_millis(1).await;
        }
    }
}

fn format_serial(id: u64) -> &'static str {
    static SERIAL: StaticCell<[u8; 16]> = StaticCell::new();
    let hex = SERIAL.init_with(|| {
        core::array::from_fn(|i| b"0123456789ABCDEF"[((id >> (60 - 4 * i)) & 0xF) as usize])
    });
    unsafe { core::str::from_utf8_unchecked(hex) }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // 1. Configure 1.25V core voltage, 252 MHz PLL_SYS (1512 MHz VCO / 6 / 1), and 48 MHz PLL_USB
    let mut cfg = embassy_rp::config::Config::default();
    cfg.clocks = ClockConfig::crystal(12_000_000);
    cfg.clocks.core_voltage = CoreVoltage::V1_25;
    if let Some(xosc) = cfg.clocks.xosc.as_mut() {
        xosc.sys_pll = Some(PllConfig {
            refdiv: 1,
            fbdiv: 126,
            post_div1: 6,
            post_div2: 1,
        });
    }

    let p = embassy_rp::init(cfg);
    info!("Embassy initialized at 252 MHz (PLL_USB: 48 MHz)");

    let mut led = Output::new(p.PIN_25, Level::High);
    debug!("LED turned on");

    // 2. Preload frame 0 and frame 1 into SRAM double-buffer before starting Core 1
    let total_vid_frames = (BAD_APPLE_DATA.len() / FB_BYTES).max(1);
    unsafe {
        let bufs = &mut *FB_BUFS.0.get();
        if BAD_APPLE_DATA.len() >= FB_BYTES {
            bufs[0].copy_from_slice(&BAD_APPLE_DATA[0..FB_BYTES]);
        }
        let f1_start = (1 % total_vid_frames) * FB_BYTES;
        if f1_start + FB_BYTES <= BAD_APPLE_DATA.len() {
            bufs[1].copy_from_slice(&BAD_APPLE_DATA[f1_start..f1_start + FB_BYTES]);
        }
    }
    FRAME_READY.store(true, Ordering::Release);

    // 3. Initialize HSTX and spawn Core 1 to drive the DVI DMA ping-pong loop
    init_hstx();
    debug!("HSTX enabled");

    static CORE1_STACK: StaticCell<Stack<8192>> = StaticCell::new();
    spawn_core1(p.CORE1, CORE1_STACK.init(Stack::new()), core1_dvi_loop);
    led.set_low();
    info!("Core 1 spawned for steady-state DVI DMA loop");

    // 4. Configure USB (CDC-ACM CLI + Picotool Reset Interface)
    let serial_str = format_serial(embassy_rp::otp::get_chipid().unwrap_or(0));

    let mut usb_cfg = UsbConfig::new(0x2E8A, 0x000A);
    usb_cfg.manufacturer = Some("Raspberry Pi");
    usb_cfg.product = Some("RP2350 Logic Analyzer (DVI)");
    usb_cfg.serial_number = Some(serial_str);
    usb_cfg.device_release = 0x0100;
    usb_cfg.max_power = 100;
    usb_cfg.max_packet_size_0 = 64;
    usb_cfg.device_class = 0xEF;
    usb_cfg.device_sub_class = 0x02;
    usb_cfg.device_protocol = 0x01;
    usb_cfg.composite_with_iads = true;

    static CFG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CTRL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

    let driver = Driver::new(p.USB, Irqs);
    let mut builder = Builder::new(
        driver,
        usb_cfg,
        CFG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CTRL_BUF.init([0; 64]),
    );

    let reset_str = builder.string();

    // Interfaces 0 & 1: CDC-ACM Interactive CLI
    static CDC_STATE: StaticCell<CdcState<'static>> = StaticCell::new();
    let cdc = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), 64);

    // Interface 2: Picotool Reset Interface
    let reset_itf_num = {
        let mut func = builder.function(
            reset_iface::RPI_RESET_INTERFACE_CLASS,
            reset_iface::RPI_RESET_INTERFACE_SUBCLASS,
            reset_iface::RPI_RESET_INTERFACE_PROTOCOL,
        );
        let mut iface = func.interface();
        let itf_num = iface.interface_number();
        let _alt = iface.alt_setting(
            reset_iface::RPI_RESET_INTERFACE_CLASS,
            reset_iface::RPI_RESET_INTERFACE_SUBCLASS,
            reset_iface::RPI_RESET_INTERFACE_PROTOCOL,
            Some(reset_str),
        );
        itf_num
    };

    static RESET_HANDLER: StaticCell<reset_iface::ResetHandler> = StaticCell::new();
    builder.handler(RESET_HANDLER.init(reset_iface::ResetHandler::new(reset_itf_num, reset_str)));

    let mut usb = builder.build();

    join3(usb.run(), usb_cli::cdc_cli_task(cdc), frame_prefetch_task()).await;
}
