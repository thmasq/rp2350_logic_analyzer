//! DVI timing and command-list generation for RP2350 HSTX.
//!
//! Peripheral setup (clocks, pin roles, TMDS encoder) is handled directly
//! via `rp235x_hal::hstx::Hstx` in main.rs. This module only builds the
//! constant command-list data that gets DMA'd into the HSTX FIFO.

use rp235x_hal::hstx::{EXPANDER_CMD_NOP, EXPANDER_CMD_RAW_REPEAT, EXPANDER_CMD_TMDS};

/// Supported color formats for the hardware TMDS encoder.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum ColorFormat {
    Rgb332,
    Rgb565,
}

impl ColorFormat {
    /// (rotation, nbits-1) for (Lane0/Blue, Lane1/Green, Lane2/Red),
    /// for passing straight into `Hstx::configure_tmds`.
    pub const fn tmds_lanes_config(&self) -> ((u8, u8), (u8, u8), (u8, u8)) {
        match self {
            Self::Rgb332 => ((26, 1), (29, 2), (0, 2)),
            Self::Rgb565 => ((0, 4), (5, 5), (11, 4)),
        }
    }
}

/// Defines the video timing specification.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct DviTiming {
    pub h_front_porch: u32,
    pub h_sync_width: u32,
    pub h_back_porch: u32,
    pub h_active_pixels: u32,
    pub v_front_porch: u32,
    pub v_sync_width: u32,
    pub v_back_porch: u32,
    pub v_active_lines: u32,
}

impl DviTiming {
    pub const VGA_640X480_60HZ: Self = Self {
        h_front_porch: 16,
        h_sync_width: 96,
        h_back_porch: 48,
        h_active_pixels: 640,
        v_front_porch: 10,
        v_sync_width: 2,
        v_back_porch: 33,
        v_active_lines: 480,
    };
}

/// Pre-calculated HSTX command-expander line buffers.
pub struct DviLineCommands {
    pub vblank_vsync_on: [u32; 7],
    pub vblank_vsync_off: [u32; 7],
    pub vactive: [u32; 7],
}

impl DviLineCommands {
    const TMDS_CTRL_00: u32 = 0x354;
    const TMDS_CTRL_01: u32 = 0x0ab;
    const TMDS_CTRL_10: u32 = 0x154;
    const TMDS_CTRL_11: u32 = 0x2ab;

    const SYNC_V0_H0: u32 =
        Self::TMDS_CTRL_00 | (Self::TMDS_CTRL_00 << 10) | (Self::TMDS_CTRL_00 << 20);
    const SYNC_V0_H1: u32 =
        Self::TMDS_CTRL_01 | (Self::TMDS_CTRL_00 << 10) | (Self::TMDS_CTRL_00 << 20);
    const SYNC_V1_H0: u32 =
        Self::TMDS_CTRL_10 | (Self::TMDS_CTRL_00 << 10) | (Self::TMDS_CTRL_00 << 20);
    const SYNC_V1_H1: u32 =
        Self::TMDS_CTRL_11 | (Self::TMDS_CTRL_00 << 10) | (Self::TMDS_CTRL_00 << 20);

    pub const fn new(timing: &DviTiming) -> Self {
        Self {
            vblank_vsync_on: [
                EXPANDER_CMD_RAW_REPEAT | timing.h_front_porch,
                Self::SYNC_V0_H1,
                EXPANDER_CMD_RAW_REPEAT | timing.h_sync_width,
                Self::SYNC_V0_H0,
                EXPANDER_CMD_RAW_REPEAT | (timing.h_back_porch + timing.h_active_pixels),
                Self::SYNC_V0_H1,
                EXPANDER_CMD_NOP,
            ],
            vblank_vsync_off: [
                EXPANDER_CMD_RAW_REPEAT | timing.h_front_porch,
                Self::SYNC_V1_H1,
                EXPANDER_CMD_RAW_REPEAT | timing.h_sync_width,
                Self::SYNC_V1_H0,
                EXPANDER_CMD_RAW_REPEAT | (timing.h_back_porch + timing.h_active_pixels),
                Self::SYNC_V1_H1,
                EXPANDER_CMD_NOP,
            ],
            vactive: [
                EXPANDER_CMD_RAW_REPEAT | timing.h_front_porch,
                Self::SYNC_V1_H1,
                EXPANDER_CMD_RAW_REPEAT | timing.h_sync_width,
                Self::SYNC_V1_H0,
                EXPANDER_CMD_RAW_REPEAT | timing.h_back_porch,
                Self::SYNC_V1_H1,
                EXPANDER_CMD_TMDS | timing.h_active_pixels,
            ],
        }
    }
}
