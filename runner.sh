#!/usr/bin/env bash
set -euo pipefail

BIN_NAME="rp2350_logic_analyzer"
PROFILE="release"
TARGET="thumbv8m.main-none-eabihf"
CHIP="RP235x"
ELF="target/${TARGET}/${PROFILE}/${BIN_NAME}"

# Optional: pin a specific debug probe (e.g. "2e8a:000c-0:E66178758B114022")
# Can also be set via PROBE=... env var or --probe <id>
PROBE="${PROBE:-}"

# Ensure ~/.cargo/bin is on PATH
if [[ -d "${HOME}/.cargo/bin" && ":${PATH}:" != *":${HOME}/.cargo/bin:"* ]]; then
	PATH="${HOME}/.cargo/bin:${PATH}"
fi

usage() {
	cat <<EOF
Usage: $0 <command> [options]
       $0 <path-to-elf>                (used automatically by 'cargo run')

Commands:
  run [--probe <id>]                    Build, flash, and stream defmt RTT logs
  flash [--probe <id>]                  Build and flash (resets MCU, leaves running)
  attach [--no-reset] [--probe <id>]    Attach defmt RTT logger (resets unless --no-reset)
  cdc [port]                            Connect to USB CDC-ACM CLI (auto-detects 2e8a:000a ttyACM*)
  list                                  List connected debug probes and BOOTSEL devices
  help                                  Show this help message

Examples:
  $0 flash			# Build & flash (auto-selects probe-rs or picotool)
  $0 run			# Build, flash, and stream RTT logs
  $0 attach --no-reset      # Attach RTT logs to running target without resetting
  $0 cdc			# Open interactive CDC-ACM CLI
EOF
	exit 1
}

build_fw() {
	echo "==> Building ${BIN_NAME} (${TARGET}, ${PROFILE})..."
	cargo build --release --target "$TARGET"
	[[ -f "$ELF" ]] || {
		echo "Error: ELF not found at $ELF" >&2
		exit 1
	}
}

probe_args() {
	PROBE_ARGS=()
	if [[ -n "$PROBE" ]]; then
		PROBE_ARGS=(--probe "$PROBE")
	fi
}

have_probe() {
	command -v probe-rs >/dev/null 2>&1 || return 1
	[[ -n "$PROBE" ]] && return 0

	local dev vid pid prod
	for dev in /sys/bus/usb/devices/*; do
		[[ -f "$dev/idVendor" ]] || continue
		vid=$(<"$dev/idVendor")
		pid=$(<"$dev/idProduct")
		prod=""
		[[ -f "$dev/product" ]] && prod=$(<"$dev/product")
		if [[ ("$vid" == "2e8a" && ("$pid" == "000c" || "$pid" == "0004")) || "$prod" == *"CMSIS-DAP"* ]]; then
			return 0
		fi
	done

	probe-rs list 2>/dev/null | grep -qE '^\[[0-9]+\]'
}

have_bootsel() {
	command -v picotool >/dev/null 2>&1 &&
		picotool info >/dev/null 2>&1
}

# Parse [--probe <id>] [--no-reset]
parse_args() {
	RESET=true

	while [[ $# -gt 0 ]]; do
		case "$1" in
		--no-reset | no-reset)
			RESET=false
			shift
			;;
		--probe)
			[[ $# -ge 2 ]] || {
				echo "Error: --probe requires a probe ID argument" >&2
				exit 1
			}
			PROBE="$2"
			shift 2
			;;
		--probe=*)
			PROBE="${1#--probe=}"
			shift
			;;
		-h | --help)
			usage
			;;
		*)
			echo "Unknown argument '$1'." >&2
			usage
			;;
		esac
	done

	probe_args
}

run_elf_direct() {
	local elf="$1"
	probe_args

	if have_probe; then
		echo "==> Running $elf on $CHIP via debug probe..."
		exec probe-rs run --chip "$CHIP" "${PROBE_ARGS[@]+"${PROBE_ARGS[@]}"}" "$elf"
	elif have_bootsel; then
		echo "==> Flashing $elf on $CHIP via BOOTSEL (RTT unavailable over BOOTSEL)..."
		exec picotool load -u -v -x -t elf "$elf"
	else
		echo "Error: No debug probe or BOOTSEL device found." >&2
		exit 1
	fi
}

cmd_run() {
	parse_args "$@"
	build_fw
	run_elf_direct "$ELF"
}

cmd_flash() {
	parse_args "$@"
	build_fw

	if have_probe; then
		echo "==> Flashing $ELF to $CHIP via debug probe..."
		probe-rs download --chip "$CHIP" "${PROBE_ARGS[@]+"${PROBE_ARGS[@]}"}" "$ELF"
		probe-rs reset --chip "$CHIP" "${PROBE_ARGS[@]+"${PROBE_ARGS[@]}"}"
		echo "==> $BIN_NAME flashed and restarted."
	elif have_bootsel; then
		echo "==> Flashing $ELF to $CHIP via BOOTSEL..."
		picotool load -u -v -x -t elf "$ELF"
		echo "==> $BIN_NAME flashed and restarted."
	else
		echo "Error: No debug probe or BOOTSEL device found." >&2
		exit 1
	fi
}

cmd_attach() {
	parse_args "$@"

	if ! have_probe; then
		echo "Error: No debug probe found (RTT requires a debug probe)." >&2
		exit 1
	fi

	if [[ ! -f "$ELF" ]]; then
		echo "==> ELF not found at $ELF. Building first..."
		build_fw
	fi

	if [[ "$RESET" = true ]]; then
		echo "==> Resetting $CHIP..."
		probe-rs reset --chip "$CHIP" "${PROBE_ARGS[@]+"${PROBE_ARGS[@]}"}"
	else
		echo "==> Attaching to live $CHIP without resetting..."
	fi

	echo "==> Streaming defmt RTT from $ELF..."
	exec probe-rs attach --chip "$CHIP" "${PROBE_ARGS[@]+"${PROBE_ARGS[@]}"}" "$ELF"
}

find_cdc_port() {
	local tty vid pid
	for tty in /sys/class/tty/ttyACM*; do
		[[ -e "$tty" ]] || continue
		vid=$(cat "$tty/device/../idVendor" 2>/dev/null || true)
		pid=$(cat "$tty/device/../idProduct" 2>/dev/null || true)
		if [[ "$vid" == "2e8a" && "$pid" == "000a" ]]; then
			echo "/dev/$(basename "$tty")"
			return 0
		fi
	done
	return 1
}

cmd_cdc() {
	local port="${1:-}"
	if [[ -z "$port" ]]; then
		port=$(find_cdc_port || true)
	fi

	if [[ -z "$port" || ! -c "$port" ]]; then
		echo "Error: CDC port (2e8a:000a) not found. Is the board connected via USB?" >&2
		exit 1
	fi

	echo "==> Connecting to $port (CDC-ACM CLI, Ctrl+C or Ctrl+] to exit)..."
	exec python3 -c '
import os, select, sys, termios, tty

port = sys.argv[1]
fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
os.set_blocking(fd, True)
attrs = termios.tcgetattr(fd)
attrs[0] = 0
attrs[1] = 0
attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
attrs[3] = 0
attrs[4] = termios.B115200
attrs[5] = termios.B115200
termios.tcsetattr(fd, termios.TCSANOW, attrs)

stdin_fd = sys.stdin.fileno()
stdout_fd = sys.stdout.fileno()
old_tty = termios.tcgetattr(stdin_fd)

try:
    tty.setraw(stdin_fd)
    while True:
        r, _, _ = select.select([stdin_fd, fd], [], [])                                                                                                                                                                                                 
        if stdin_fd in r:                                                                                                                                                                                                                               
            data = os.read(stdin_fd, 64)                                                                                                                                                                                                                
            if not data or b"\x1d" in data or b"\x03" in data:                                                                                                                                                                                          
                break                                                                                                                                                                                                                                   
            os.write(fd, data)                                                                                                                                                                                                                          
        if fd in r:                                                                                                                                                                                                                                     
            data = os.read(fd, 1024)                                                                                                                                                                                                                    
            if not data:                                                                                                                                                                                                                                
                break                                                                                                                                                                                                                                   
            os.write(stdout_fd, data)                                                                                                                                                                                                                   
finally:
    termios.tcsetattr(stdin_fd, termios.TCSADRAIN, old_tty)
    os.close(fd)
' "$port"
}

cmd_list() {
	local found=false
	if command -v probe-rs >/dev/null 2>&1; then
		echo "==> Debug probes (probe-rs):"
		probe-rs list || true
		found=true
	fi
	if command -v picotool >/dev/null 2>&1; then
		echo "==> BOOTSEL devices (picotool):"
		picotool info || echo "  (none in BOOTSEL mode)"
		found=true
	fi
	if [[ "$found" = false ]]; then
		echo "Error: Neither probe-rs nor picotool found on PATH." >&2
		exit 1
	fi
}

case "${1:-}" in
run)
	shift
	cmd_run "$@"
	;;
flash)
	shift
	cmd_flash "$@"
	;;
attach)
	shift
	cmd_attach "$@"
	;;
cdc)
	cmd_cdc "${2:-}"
	;;
list)
	cmd_list
	;;
help | -h | --help | "")
	usage
	;;
*)
	# Invoked by `cargo run` as `<runner> <elf>`
	if [[ -f "$1" ]]; then
		run_elf_direct "$1"
	else
		usage
	fi
	;;
esac
