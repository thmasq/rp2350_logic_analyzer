#![no_std]
#![no_main]

mod dvi;

use defmt_rtt as _;
use embedded_hal::digital::OutputPin;
use panic_probe as _;

use defmt::{debug, info};
use rp235x_hal::fugit::RateExtU32;
use rp235x_hal::{
    clocks::ClocksManager,
    dma::{DMAExt, double_buffer},
    gpio::Pins,
    hstx::{Hstx, HstxBitConfig, HstxPins},
    pac,
    pll::{PLLConfig, common_configs::PLL_USB_48MHZ, setup_pll_blocking},
    sio::Sio,
    watchdog::Watchdog,
    xosc::setup_xosc_blocking,
};

use dvi::{ColorFormat, DviLineCommands, DviTiming};

use rp235x_hal as hal;

#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [hal::binary_info::EntryAddr; 5] = [
    hal::binary_info::rp_cargo_bin_name!(),
    hal::binary_info::rp_cargo_version!(),
    hal::binary_info::rp_program_description!(c"DVI Checkerboard"),
    hal::binary_info::rp_cargo_homepage_url!(),
    hal::binary_info::rp_program_build_attribute!(),
];

// Generate our command lists at compile-time.
static DVI_COMMANDS: DviLineCommands = DviLineCommands::new(&DviTiming::VGA_640X480_60HZ);

// 640 pixels per line, RGB332 (1 byte/pixel) => 640 bytes = 160 u32s.
// Built entirely at compile time -- no runtime mutation, no `static mut`.
const fn build_checkerboard(start_red: bool) -> [u32; 160] {
    const RED_BLACK: u32 = 0x00_E0_00_E0;
    const BLACK_RED: u32 = 0xE0_00_E0_00;
    let mut arr = [0u32; 160];
    let mut i = 0;
    while i < 160 {
        let is_red_chunk = (i / 4) % 2 == 0;
        arr[i] = if is_red_chunk == start_red {
            RED_BLACK
        } else {
            BLACK_RED
        };
        i += 1;
    }
    arr
}

static PIXELS_EVEN: [u32; 160] = build_checkerboard(true);
static PIXELS_ODD: [u32; 160] = build_checkerboard(false);

// 10 VBLANK_OFF + 2 VBLANK_ON + 33 VBLANK_OFF + (480 active lines * 2 blocks/line)
const V_BLANK_OFF_FRONT: usize = 10;
const V_SYNC_ON: usize = 2;
const V_BLANK_TOTAL: usize = 45; // 10 + 2 + 33
const NUM_BLOCKS: usize = V_BLANK_TOTAL + (480 * 2);

/// Returns the DMA source slice for block index `i` (0..NUM_BLOCKS) of one frame.
fn block_for_index(i: usize) -> &'static [u32] {
    if i < V_BLANK_OFF_FRONT {
        &DVI_COMMANDS.vblank_vsync_off
    } else if i < V_BLANK_OFF_FRONT + V_SYNC_ON {
        &DVI_COMMANDS.vblank_vsync_on
    } else if i < V_BLANK_TOTAL {
        &DVI_COMMANDS.vblank_vsync_off
    } else {
        let active_idx = i - V_BLANK_TOTAL; // 0..960
        let y = active_idx / 2;
        if active_idx % 2 == 0 {
            &DVI_COMMANDS.vactive
        } else if (y / 16) % 2 == 0 {
            &PIXELS_EVEN
        } else {
            &PIXELS_ODD
        }
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    info!("Program started!");

    let mut pac = pac::Peripherals::take().unwrap();
    let mut _core = cortex_m::Peripherals::take().unwrap();
    let _watchdog = Watchdog::new(pac.WATCHDOG);
    let sio = Sio::new(pac.SIO);

    let pins = Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // 1. TURN ON LED IMMEDIATELY -- confirms we reached main() and didn't crash before here.
    let mut led = pins.gpio25.into_push_pull_output();
    led.set_high().unwrap();
    debug!("LED turned on");

    // 2. Bump core voltage so we don't brown out at 252 MHz.
    pac.POWMAN.vreg_ctrl().modify(|_, w| w.unlock().set_bit());
    pac.POWMAN
        .vreg()
        .modify(|_, w| unsafe { w.vsel().bits(0b01110) });
    for _ in 0..10_000 {
        cortex_m::asm::nop();
    }
    debug!("Core voltage bumped");

    // 3. Clock setup for exactly 252 MHz (25.2 MHz pixel clock * 10).
    let xosc = setup_xosc_blocking(pac.XOSC, 12_000_000u32.Hz()).unwrap();

    let pll_sys_252mhz = PLLConfig {
        vco_freq: 1512_000_000u32.Hz(),
        refdiv: 1,
        post_div1: 6,
        post_div2: 1,
    };

    let mut clocks = ClocksManager::new(pac.CLOCKS);

    let pll_sys = setup_pll_blocking(
        pac.PLL_SYS,
        xosc.operating_frequency(),
        pll_sys_252mhz,
        &mut clocks,
        &mut pac.RESETS,
    )
    .unwrap();

    let pll_usb = setup_pll_blocking(
        pac.PLL_USB,
        xosc.operating_frequency(),
        PLL_USB_48MHZ,
        &mut clocks,
        &mut pac.RESETS,
    )
    .unwrap();

    clocks.init_default(&xosc, &pll_sys, &pll_usb).unwrap();
    info!("Clocks configured successfully (VCO: 1512 MHz)");

    // 4. HSTX pins (Pico DVI Sock pinout). `Hstx::new()` handles the HSTX reset internally.
    let hstx_pins = HstxPins::new()
        .add_bit0_pin(pins.gpio12.into_function())
        .add_bit1_pin(pins.gpio13.into_function())
        .add_bit2_pin(pins.gpio14.into_function())
        .add_bit3_pin(pins.gpio15.into_function())
        .add_bit4_pin(pins.gpio16.into_function())
        .add_bit5_pin(pins.gpio17.into_function())
        .add_bit6_pin(pins.gpio18.into_function())
        .add_bit7_pin(pins.gpio19.into_function());

    let mut hstx = Hstx::new(pac.HSTX_CTRL, pac.HSTX_FIFO, hstx_pins, &mut pac.RESETS);

    // Serial output: clock period of 5 cycles, shift the output shiftreg by 2 bits/cycle
    // (5 * 2 = one 10-bit TMDS symbol per lane, DDR).
    hstx.configure_clk(5, 0);
    hstx.configure_shifter(2, 5);

    // Pixels arrive as 4 packed 8-bit chunks per 32-bit FIFO word; sync/control words
    // (RAW) are consumed whole, once per FIFO word.
    hstx.configure_expand_shifter(8, 4, 0, 1);

    let (l0, l1, l2) = ColorFormat::Rgb332.tmds_lanes_config();
    hstx.configure_tmds(l0.0, l0.1, l1.0, l1.1, l2.0, l2.1);

    // bit0/1 = D0, bit2/3 = CLK, bit4/5 = D2, bit6/7 = D1 (Pico DVI Sock pinout).
    // sel_p/sel_n = (lane*10, lane*10+1) to step through that lane's 10-bit TMDS symbol.
    hstx.configure_bit0(HstxBitConfig::Shift {
        sel_p: 0,
        sel_n: 1,
        inv: false,
    });
    hstx.configure_bit1(HstxBitConfig::Shift {
        sel_p: 0,
        sel_n: 1,
        inv: true,
    });
    hstx.configure_bit2(HstxBitConfig::Clk { inv: false });
    hstx.configure_bit3(HstxBitConfig::Clk { inv: true });
    hstx.configure_bit4(HstxBitConfig::Shift {
        sel_p: 20,
        sel_n: 21,
        inv: false,
    });
    hstx.configure_bit5(HstxBitConfig::Shift {
        sel_p: 20,
        sel_n: 21,
        inv: true,
    });
    hstx.configure_bit6(HstxBitConfig::Shift {
        sel_p: 10,
        sel_n: 11,
        inv: false,
    });
    hstx.configure_bit7(HstxBitConfig::Shift {
        sel_p: 10,
        sel_n: 11,
        inv: true,
    });

    hstx.enable_expander();
    let hstx = hstx.enable();
    debug!("HSTX enabled");

    // 5. DMA: hardware ping-pong via chain_to, driven by the double-buffer API.
    // `dma.split()` handles the DMA subsystem reset internally.
    let dma = pac.DMA.split(&mut pac.RESETS);

    let mut idx: usize = 0;
    let xfer = double_buffer::Config::new((dma.ch0, dma.ch1), block_for_index(idx), hstx).start();
    idx += 1;
    let mut xfer = xfer.read_next(block_for_index(idx));
    idx += 1;

    // Turn LED off just before entering the steady-state loop.
    led.set_low().unwrap();
    info!("Setup complete, entering steady-state DMA loop");

    // 6. Steady state: each iteration blocks until the active channel completes (which,
    // thanks to hardware chain_to, means the other channel has *already* started), then
    // arms the channel that just finished with the next block in the sequence.
    loop {
        let (_finished_buf, next) = xfer.wait();
        xfer = next.read_next(block_for_index(idx));
        idx = (idx + 1) % NUM_BLOCKS;
    }
}
