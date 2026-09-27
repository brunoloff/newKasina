#!/usr/bin/env python3
"""Reproduce the bundled English counts using eSpeak NG and ffmpeg (not runtime dependencies)."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / 'crates/kasina-counting/assets/voices'
WORDS = 'one two three four five six seven eight nine ten'.split()
VOICES = [('en-us+m3', 40), ('en-us+f2', 50), ('en-gb+m1', 43), ('en-us+f3', 42)]
DEST.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory() as temporary:
    wav = Path(temporary) / 'number.wav'
    for index, (voice, pitch) in enumerate(VOICES):
        for number, word in enumerate(WORDS, 1):
            subprocess.run(['espeak-ng', '-v', voice, '-s', '135', '-p', str(pitch), '-a', '130', '-w', str(wav), word], check=True)
            subprocess.run(['ffmpeg', '-v', 'error', '-y', '-i', str(wav), '-ar', '16000', '-ac', '1', '-af',
                            'silenceremove=start_periods=1:start_threshold=-45dB:stop_periods=-1:stop_threshold=-45dB,apad=pad_dur=0.08',
                            str(DEST / f'{index}-{number}.wav')], check=True)
