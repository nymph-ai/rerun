"""Encode a synthetic sine wave as Opus and stream it into Rerun."""

import av
import numpy as np

import rerun as rr

sample_rate = 48000
duration_seconds = 4
frequency = 440.0  # A4

rr.init("rerun_example_audio_stream_synthetic", spawn=True)

# Static configuration — codec, sample rate, channel count stay constant for the life of the stream.
rr.log(
    "audio_stream",
    rr.AudioStream(codec=rr.components.AudioCodec.Opus, sample_rate=sample_rate, channel_count=1),
    static=True,
)

# Opus encode a sine tone via PyAV.
container = av.open("/dev/null", "w", format="ogg")
stream = container.add_stream("libopus", rate=sample_rate)
# Type narrowing.
assert isinstance(stream, av.audio.stream.AudioStream)
stream.sample_rate = sample_rate
stream.layout = "mono"

total_samples = int(sample_rate * duration_seconds)
t = np.arange(total_samples, dtype=np.float32) / sample_rate
samples = np.sin(2 * np.pi * frequency * t)

# Opus works in fixed-size frames. 20 ms @ 48 kHz = 960 samples per frame.
frame_size = 960
for start in range(0, total_samples, frame_size):
    end = min(start + frame_size, total_samples)
    block = samples[start:end].reshape(1, -1)
    frame = av.AudioFrame.from_ndarray(block, format="flt", layout="mono")
    frame.sample_rate = sample_rate
    frame.pts = start
    for packet in stream.encode(frame):
        if packet.pts is None:
            continue
        pts_seconds = float(packet.pts * packet.time_base)
        rr.set_time("time", duration=pts_seconds)
        rr.log(
            "audio_stream",
            rr.AudioStream.from_fields(chunk=bytes(packet), duration_samples=packet.duration),
        )

for packet in stream.encode():
    if packet.pts is None:
        continue
    pts_seconds = float(packet.pts * packet.time_base)
    rr.set_time("time", duration=pts_seconds)
    rr.log(
        "audio_stream",
        rr.AudioStream.from_fields(chunk=bytes(packet), duration_samples=packet.duration),
    )
