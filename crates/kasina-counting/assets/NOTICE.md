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

The sixty WAV files are prerecorded synthetic speech generated with Kokoro-82M
v1.0 by hexgrad (model license: Apache-2.0), using kokoro-onnx 0.5.0 (MIT).
The voice bank contains Michael, Heart, Puck, Sarah, Fenrir, and Bella; these
are the model's stock voice presets, not custom clones. The model and TTS engine
are not bundled or run by newKasina. Only the generated audio is included.

Reproduce the clips with scripts/generate-counting-voices.py. The script pins
the input model and voice-bank hashes, and voices/manifest.json records the
output clip hashes. Python, ONNX Runtime, the model, and ffmpeg are asset-authoring
tools only. The generated recordings and synthesized bell are included under
this project's MIT OR Apache-2.0 license.

Sources:
- https://huggingface.co/hexgrad/Kokoro-82M
- https://huggingface.co/hexgrad/Kokoro-82M/blob/main/VOICES.md
- https://github.com/thewh1teagle/kokoro-onnx
- https://github.com/thewh1teagle/kokoro-onnx/releases/tag/model-files-v1.1
