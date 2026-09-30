# Offline speech assets

The English speech model is OpenAI Whisper tiny.en, quantized to q5_1 by
whisper.cpp. The downloaded file, immutable revision, byte count and SHA-256
are pinned in model.json. Model weights are distributed under the MIT license.
Sources: https://github.com/openai/whisper and https://huggingface.co/ggerganov/whisper.cpp

Copyright (c) 2022 OpenAI

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

The sixty WAV files are prerecorded synthetic speech generated with Qwen3-TTS
by the Qwen team (Apache-2.0). The two approved synthetic audition recordings
and four additional synthetic voice references are in voice-references/.
Qwen3-TTS-12Hz-1.7B-VoiceDesign created those references;
Qwen3-TTS-12Hz-1.7B-Base used them to generate consistent counting takes.
No real person's voice recording was used as a reference.

The model weights and TTS engine are not bundled or run by newKasina. Only the
number clips are embedded in the app. Python, PyTorch, Qwen3-TTS, and ffmpeg are
asset-authoring tools only. The generated recordings and synthesized bell are
included under this project's MIT OR Apache-2.0 license.

Reproduce the clips with scripts/generate-counting-voices.py. Immutable model
revisions, reference hashes, take hashes, and final clip hashes are recorded in
voices/manifest.json. The approved references must be preserved when regenerating.

Sources:
- https://github.com/QwenLM/Qwen3-TTS
- https://huggingface.co/Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign
- https://huggingface.co/Qwen/Qwen3-TTS-12Hz-1.7B-Base
