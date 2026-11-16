use core::fmt::Write as FmtWrite;
use embassy_rp::clocks::{clk_sys_freq, clk_usb_freq};
use embassy_time::Instant;
use embassy_usb::class::cdc_acm::{CdcAcmClass, Sender};
use heapless::String;
use portable_atomic::Ordering;

use crate::{DVI_FRAMES, UsbDriver, VIDEO_FRAME};

async fn write_str(sender: &mut Sender<'static, UsbDriver>, text: &str) {
    for chunk in text.as_bytes().chunks(64) {
        if sender.write_packet(chunk).await.is_err() {
            break;
        }
    }
}

async fn handle_cli_command(cmd_line: &str, sender: &mut Sender<'static, UsbDriver>) {
    let mut parts = cmd_line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return;
    };

    let mut out: String<512> = String::new();

    match cmd {
        "help" | "?" => {
            write_str(
                sender,
                "\r\n--- RP2350 Logic Analyzer CLI ---\r\n\
                         status   : Show clock frequencies, uptime & DVI frame count\r\n\
                         bootsel  : Reboot into USB BOOTSEL mode\r\n\
                         help     : Show this help message\r\n",
            )
            .await;
            return;
        }
        "status" => {
            let frames = DVI_FRAMES.load(Ordering::Relaxed);
            let video_frame = VIDEO_FRAME.load(Ordering::Relaxed);
            let uptime_ms = Instant::now().as_millis();
            let _ = write!(
                out,
                "\r\n[Status]\r\n\
                         clk_sys    : {} Hz\r\n\
                         clk_usb    : {} Hz\r\n\
                         clk_hstx   : {} Hz\r\n\
                         DVI Frames : {}\r\n\
                         Video Frm  : {}\r\n\
                         Uptime     : {}.{:03} s\r\n",
                clk_sys_freq(),
                clk_usb_freq(),
                clk_sys_freq() / 2,
                frames,
                video_frame,
                uptime_ms / 1000,
                uptime_ms % 1000,
            );
        }
        "bootsel" => {
            write_str(sender, "\r\nRebooting to BOOTSEL...\r\n").await;
            embassy_rp::rom_data::reset_to_usb_boot(0, 0);
        }
        _ => {
            let _ = write!(
                out,
                "\r\nUnknown command '{}'. Type 'help' for commands.\r\n",
                cmd
            );
        }
    }

    write_str(sender, &out).await;
}

pub async fn cdc_cli_task(cdc: CdcAcmClass<'static, UsbDriver>) -> ! {
    let (mut sender, mut receiver) = cdc.split();

    loop {
        receiver.wait_connection().await;
        write_str(
            &mut sender,
            "\r\n=== RP2350 Logic Analyzer Ready (Type 'help') ===\r\n> ",
        )
        .await;

        let mut rx_buf = [0u8; 64];
        let mut cmd_line: String<128> = String::new();
        let mut last_was_cr = false;

        loop {
            match receiver.read_packet(&mut rx_buf).await {
                Ok(0) => {}
                Ok(n) => {
                    for &b in &rx_buf[..n] {
                        match b {
                            b'\r' | b'\n' => {
                                if b == b'\n' && last_was_cr {
                                    last_was_cr = false;
                                    continue;
                                }
                                last_was_cr = b == b'\r';

                                write_str(&mut sender, "\r\n").await;
                                let trimmed = cmd_line.trim();
                                if !trimmed.is_empty() {
                                    handle_cli_command(trimmed, &mut sender).await;
                                }
                                cmd_line.clear();
                                write_str(&mut sender, "> ").await;
                            }
                            0x08 | 0x7F => {
                                last_was_cr = false;
                                if cmd_line.pop().is_some() {
                                    write_str(&mut sender, "\x08 \x08").await;
                                }
                            }
                            c if c.is_ascii_graphic() || c == b' ' => {
                                last_was_cr = false;
                                if cmd_line.push(c as char).is_ok() {
                                    let _ = sender.write_packet(&[c]).await;
                                }
                            }
                            _ => {
                                last_was_cr = false;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }
}
