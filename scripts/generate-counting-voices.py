#!/usr/bin/env python3
"""Generate bundled counts with Kokoro v1.0; never invoked by app or Cargo builds.

Python 3.12: pip install kokoro-onnx==0.5.0 soundfile==0.14.0
Also requires ffmpeg. Download the two pinned files from:
https://github.com/thewh1teagle/kokoro-onnx/releases/tag/model-files-v1.1
Then run: python scripts/generate-counting-voices.py /path/to/model-files
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

import numpy as np
import onnxruntime as ort
import soundfile as sf
from kokoro_onnx import Kokoro

ROOT = Path(__file__).resolve().parent.parent
WORDS = 'one two three four five six seven eight nine ten'.split()
# Stable indices are persisted in the app's per-companion voice selections.
VOICES = ['am_michael', 'af_heart', 'am_puck', 'af_sarah', 'am_fenrir', 'af_bella']
SPEED_OVERRIDES = {'2-6.wav': 1.0, '3-10.wav': 1.0}
HASHES = {
    'kokoro-v1.0.onnx': 'beb0d1848dee9a49da392cc3df26958d46cfa35d321edf434f52949153f0df3a',
    'voices-v1.0.bin': 'bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d',
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('models', type=Path)
    parser.add_argument('--output', type=Path, default=ROOT / 'crates/kasina-counting/assets/voices')
    parser.add_argument('--only', nargs='+', help='Regenerate selected filenames in an existing complete bank')
    args = parser.parse_args()
    for name, expected in HASHES.items():
        assert hashlib.sha256((args.models / name).read_bytes()).hexdigest() == expected, name
    options = ort.SessionOptions()
    options.intra_op_num_threads = 4
    options.inter_op_num_threads = 1
    ort.disable_telemetry_events()
    session = ort.InferenceSession(str(args.models / 'kokoro-v1.0.onnx'), options, providers=['CPUExecutionProvider'])
    kokoro = Kokoro.from_session(session, str(args.models / 'voices-v1.0.bin'))
    args.output.mkdir(parents=True, exist_ok=True)
    manifest = {'model': 'Kokoro-82M v1.0', 'model_sha256': HASHES, 'speed': 0.9, 'speed_overrides': SPEED_OVERRIDES, 'voices': VOICES, 'clips': {}}
    with tempfile.TemporaryDirectory() as temporary:
        intermediate = Path(temporary) / 'number.wav'
        for index, voice in enumerate(VOICES):
            for number, word in enumerate(WORDS, 1):
                filename = f'{index}-{number}.wav'
                if args.only and filename not in args.only:
                    continue
                speed = SPEED_OVERRIDES.get(filename, 0.9)
                phonemes = kokoro.tokenizer.phonemize(word.capitalize() + '.',
                    lang='en-gb' if voice.startswith('b') else 'en-us')
                tokens = kokoro.tokenizer.tokenize(phonemes)
                # This pinned export takes FLOAT speed. kokoro-onnx 0.5.0's
                # create() incorrectly casts it to INT for input_ids exports.
                samples = session.run(None, {
                    'input_ids': np.array([[0, *tokens, 0]], dtype=np.int64),
                    'style': np.asarray(kokoro.get_voice_style(voice)[len(tokens)], dtype=np.float32),
                    'speed': np.array([speed], dtype=np.float32),
                })[0].reshape(-1)
                rate = 24000
                # Trim only outer silence, keeping consonants and internal pauses.
                active = np.flatnonzero(np.abs(samples) > 0.002)
                assert len(active), (voice, word)
                samples = samples[max(0, active[0] - int(rate * 0.04)):active[-1] + 1 + int(rate * 0.08)]
                # Consistent RMS with peak headroom, then a small fade and tail.
                rms = np.sqrt(np.mean(samples ** 2))
                samples *= min(0.10 / max(rms, 1e-8), 0.85 / max(np.max(np.abs(samples)), 1e-8))
                fade = min(int(rate * 0.005), len(samples) // 2)
                samples[:fade] *= np.linspace(0, 1, fade)
                samples[-fade:] *= np.linspace(1, 0, fade)
                samples = np.pad(samples, (0, int(rate * 0.08)))
                sf.write(intermediate, samples, rate, subtype='PCM_16')
                target = args.output / f'{index}-{number}.wav'
                subprocess.run(['ffmpeg', '-v', 'error', '-y', '-i', str(intermediate),
                                '-ar', '16000', '-ac', '1', '-c:a', 'pcm_s16le', str(target)], check=True)
                audio, rate = sf.read(target)
                assert rate == 16000 and 0.15 < len(audio) / rate < 2.0, (voice, word)
                manifest['clips'][target.name] = hashlib.sha256(target.read_bytes()).hexdigest()
                print(f'{voice:12} {number:2}: {len(audio) / rate:.2f}s', flush=True)
    manifest['clips'] = {f'{v}-{n}.wav': hashlib.sha256((args.output / f'{v}-{n}.wav').read_bytes()).hexdigest()
                         for v in range(len(VOICES)) for n in range(1, 11)}
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    main()
