#!/usr/bin/env python3
"""Author prerecorded Qwen3-TTS counts; never used by the app or Cargo builds.

Python 3.12, CPU torch==2.8.0, qwen-tts==0.1.1, soundfile==0.14.0,
transformers==4.57.3, plus ffmpeg. Set HF_HOME to an authoring cache.
Run --stage references (only when adding voices), then --stage takes, then
--stage pack. Reference 0/1 are the user's approved synthetic auditions.
Models are downloaded by Hugging Face; no model weights enter the app bundle.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import numpy as np
import soundfile as sf

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / 'crates/kasina-counting/assets'
DESIGN = 'Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign'
BASE = 'Qwen/Qwen3-TTS-12Hz-1.7B-Base'
DESIGN_REVISION = '5ecdb67327fd37bb2e042aab12ff7391903235d3'
BASE_REVISION = 'fd4b254389122332181a7c3db7f27e918eec64e3'
WORDS = 'One Two Three Four Five Six Seven Eight Nine Ten'.split()
TEXT = '. '.join(WORDS) + '.'
NAMES = ['Warm male', 'Warm female', 'Low male', 'Soft female', 'Light male', 'Clear female']
# The clear female take joins three/four closely and pauses inside eight.
# Reviewed word boundaries override the automatic longest-silence heuristic.
CUTS = {5: [1.565, 2.78, 3.74, 4.57, 5.585, 6.45, 7.165, 7.795, 8.69]}
CUT_TAKE_HASHES = {5: 'b688f22c267299ed50c00ae308ae1574f8ec582e63c5646f8e5b32785144bc87'}
COMMON = (' speaks English with a neutral accent. Quiet, natural everyday speech, '
          'as if counting along with friends meditating in a quiet room. Relaxed '
          'and understated, with a comfortable short pause between each number. '
          'Normal voiced speech, not whispering. No theatrical or presenter delivery.')
PROFILES = [
    'An adult man with a gentle, warm mid-low voice who',
    'An adult woman with a gentle, warm midrange voice who',
    'A mature adult man with a naturally deep, mellow bass voice who',
    'A mature adult woman with a soft, slightly husky low-mid voice who',
    'A young adult man with a light, clear tenor voice who',
    'An adult woman with a clear, lightly bright voice who',
]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_model(identifier, revision):
    import torch
    from qwen_tts import Qwen3TTSModel
    from huggingface_hub import snapshot_download
    torch.set_num_threads(4)
    torch.set_num_interop_threads(1)
    print(f'Loading {identifier}', flush=True)
    model_path = snapshot_download(identifier, revision=revision, ignore_patterns=["*.md", ".gitattributes"])
    return Qwen3TTSModel.from_pretrained(model_path,
        device_map='cpu', dtype=torch.float32, attn_implementation='sdpa')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', choices=['references', 'takes', 'pack'], required=True)
    parser.add_argument('--takes', type=Path, default=ROOT / 'target/tts-auditions/qwen/takes')
    parser.add_argument('--output', type=Path, default=ASSETS / 'voices')
    parser.add_argument('--only', type=int, nargs='+', default=list(range(6)))
    args = parser.parse_args()
    assert all(0 <= i < 6 for i in args.only)
    references = ASSETS / 'voice-references'
    references.mkdir(parents=True, exist_ok=True)
    args.takes.mkdir(parents=True, exist_ok=True)
    if args.stage == 'references':
        import torch
        model = load_model(DESIGN, DESIGN_REVISION)
        for index in args.only:
            path = references / f'{index}.wav'
            if path.exists():
                continue
            assert index >= 2, 'Keep the two approved auditions unchanged'
            torch.manual_seed(28371 + index)
            wavs, rate = model.generate_voice_design(text='One. Two. Three.', language='English',
                instruct=PROFILES[index] + COMMON, max_new_tokens=160, temperature=0.7)
            sf.write(path, wavs[0], rate, subtype='PCM_16')
            print(f'Reference {index}: {NAMES[index]}', flush=True)
        return
    if args.stage == 'takes':
        import torch
        model = load_model(BASE, BASE_REVISION)
        for index in args.only:
            torch.manual_seed(84621 + index)
            audio, rate = sf.read(references / f'{index}.wav', dtype='float32')
            prompt = model.create_voice_clone_prompt(ref_audio=(audio, rate),
                ref_text='One. Two. Three.', x_vector_only_mode=False)
            print(f'Generating take {index}: {NAMES[index]}', flush=True)
            wavs, rate = model.generate_voice_clone(text=TEXT, language='English',
                voice_clone_prompt=prompt, max_new_tokens=450, temperature=0.7)
            sf.write(args.takes / f'{index}.wav', wavs[0], rate, subtype='PCM_16')
            print(f'Saved take {index}: {len(wavs[0])/rate:.2f}s', flush=True)
        return
    args.output.mkdir(parents=True, exist_ok=True)
    manifest = {'model': BASE, 'revision': BASE_REVISION,
        'reference_model': DESIGN, 'reference_revision': DESIGN_REVISION,
        'voices': NAMES, 'reference_text': 'One. Two. Three.', 'take_text': TEXT,
        'take_seeds': [84621 + index for index in range(6)], 'temperature': 0.7,
        'reference_instructions': [profile + COMMON for profile in PROFILES],
        'reviewed_boundaries_seconds': CUTS,
        'references': {}, 'takes': {}, 'clips': {}}
    for index in args.only:
        audio, rate = sf.read(args.takes / f'{index}.wav')
        # Find word boundaries from the nine longest internal silences. Keep
        # natural consonants and verify every resulting word with offline ASR.
        frame = rate // 100
        energy = np.array([np.sqrt(np.mean(audio[i:i+frame] ** 2))
                           for i in range(0, len(audio), frame)])
        active = energy > max(0.001, energy.max() * 0.025)
        speech = np.flatnonzero(active)
        assert len(speech)
        gaps = []
        start = None
        for i in range(speech[0], speech[-1] + 1):
            if not active[i] and start is None:
                start = i
            if active[i] and start is not None:
                gaps.append((start, i))
                start = None
        selected = sorted(sorted(gaps, key=lambda x: x[1]-x[0], reverse=True)[:9])
        assert len(selected) == 9, (index, gaps)
        boundaries = [0] + [((a+b)//2)*frame for a,b in selected] + [len(audio)]
        if index in CUTS:
            assert digest(args.takes / f'{index}.wav') == CUT_TAKE_HASHES[index], \
                'Regenerated take changed: review its word boundaries before packing'
            boundaries = [0] + [round(t * rate) for t in CUTS[index]] + [len(audio)]
        for number, (a,b) in enumerate(zip(boundaries, boundaries[1:]), 1):
            clip = audio[a:b].copy()
            on = np.flatnonzero(np.abs(clip) > 0.0015)
            assert len(on), (index, number)
            clip = clip[max(0, on[0]-int(.04*rate)):min(len(clip), on[-1]+int(.10*rate))]
            # Preserve the audition's dynamics; only protect against clipping.
            clip *= min(1.0, .95 / max(np.max(np.abs(clip)), 1e-8))
            path = args.output / f'{index}-{number}.wav'
            intermediate = args.takes / 'resample.wav'
            sf.write(intermediate, clip, rate, subtype='PCM_16')
            subprocess.run(['ffmpeg', '-v', 'error', '-y', '-i', str(intermediate),
                '-ar', '16000', '-ac', '1', '-c:a', 'pcm_s16le', str(path)], check=True)
            assert .15 < len(clip)/rate < 2, (index, number, len(clip)/rate)
            print(f'{index}-{number}: {len(clip)/rate:.2f}s', flush=True)
    for index in range(6):
        manifest['references'][str(index)] = digest(references / f'{index}.wav')
        manifest['takes'][str(index)] = digest(args.takes / f'{index}.wav')
        for number in range(1,11):
            name = f'{index}-{number}.wav'
            manifest['clips'][name] = digest(args.output / name)
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    main()
