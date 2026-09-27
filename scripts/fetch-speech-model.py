#!/usr/bin/env python3
"""Fetch the pinned, MIT-licensed English Whisper model for offline counting."""
import hashlib
import json
from pathlib import Path
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = json.loads((ROOT / 'crates/kasina-counting/assets/model.json').read_text())


def fetch():
    destination = ROOT / 'target/speech-models' / MANIFEST['filename']
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists() and valid(destination):
        return destination
    temporary = destination.with_suffix('.download')
    try:
        with urllib.request.urlopen(MANIFEST['url'], timeout=120) as response, temporary.open('wb') as output:
            total = 0
            while block := response.read(65536):
                total += len(block)
                if total > MANIFEST['bytes']:
                    raise ValueError('Speech model download is larger than expected')
                output.write(block)
        if not valid(temporary):
            raise ValueError('Speech model checksum mismatch')
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def valid(path):
    return path.stat().st_size == MANIFEST['bytes'] and hashlib.sha256(path.read_bytes()).hexdigest() == MANIFEST['sha256']


if __name__ == '__main__':
    print(fetch())
