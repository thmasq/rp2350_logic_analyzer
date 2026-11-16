import subprocess
import sys
from pathlib import Path

WIDTH = 80
HEIGHT = 60
FPS = 30
FRAME_PIXELS = WIDTH * HEIGHT
FRAME_BYTES = FRAME_PIXELS // 8  # 600 bytes per frame


def pack_frame_msb(raw_gray: bytes) -> bytes:
    """Pack 80x60 8-bit grayscale pixels into 600 MSB-first 1-bit bytes."""
    out = bytearray(FRAME_BYTES)
    for byte_idx in range(FRAME_BYTES):
        base = byte_idx * 8
        b = 0
        for bit in range(8):
            if raw_gray[base + bit] >= 128:
                b |= 0x80 >> bit
        out[byte_idx] = b
    return bytes(out)


def main() -> None:
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <input_video> <output_bin>")
        sys.exit(1)

    input_video = sys.argv[1]
    output_bin = Path(sys.argv[2])

    cmd = [
        "ffmpeg",
        "-v",
        "error",
        "-i",
        input_video,
        "-vf",
        f"fps={FPS},scale={WIDTH}:{HEIGHT}:flags=area,format=gray",
        "-f",
        "rawvideo",
        "-",
    ]

    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE)
    assert proc.stdout is not None

    raw_stream = bytearray()
    frame_count = 0

    while True:
        raw = proc.stdout.read(FRAME_PIXELS)
        if len(raw) < FRAME_PIXELS:
            break
        raw_stream.extend(pack_frame_msb(raw))
        frame_count += 1

    proc.wait()
    output_bin.write_bytes(raw_stream)

    size_mib = len(raw_stream) / (1024 * 1024)
    print(
        f"Wrote {frame_count} lossless 1bpp frames ({WIDTH}x{HEIGHT} @ {FPS}fps) "
        f"to {output_bin} ({len(raw_stream):,} bytes / {size_mib:.2f} MiB)"
    )
    if len(raw_stream) > 3_950_000:
        print(
            "Warning: close to 4 MiB flash limit; lower FPS if flashing fails.",
            file=sys.stderr,
        )


if __name__ == "__main__":
    main()
