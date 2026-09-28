#!/usr/bin/env python3
"""Make timestamped VLM contact sheets from real finalized observer/player videos (Pillow).

No generated pixels, interpolation, or player removal. Observer sheets disclose the HUD crop;
the original videos remain unchanged. Run alongside qc-actions and audit_regional_traffic.py.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
from PIL import Image, ImageDraw, ImageFont


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('dataset', type=Path)
    p.add_argument('--out', type=Path, required=True)
    a = p.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    episode = json.loads((a.dataset / 'episode.json').read_text())
    fonts = ['/usr/share/fonts/dejavu/DejaVuSans.ttf', '/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf']
    font_path = next((f for f in fonts if Path(f).exists()), None)
    font = ImageFont.truetype(font_path, 22) if font_path else ImageFont.load_default(size=22)
    provenance = []
    for pov in episode['povs']:
        name = pov['name']
        video = a.dataset / f'{name}-default.mp4'
        rows = [json.loads(s) for s in (a.dataset / f'{name}-frames.jsonl').read_text().splitlines()]
        chosen = [round((len(rows)-1)*f) for f in [0, .2, .4, .6, .8, .98]]
        sheet = Image.new('RGB', (1920, 790), 'white')
        draw = ImageDraw.Draw(sheet)
        draw.text((15, 9), f"{episode['episode_id']} / {name} / real temporal samples", fill='#172a3a', font=font)
        with tempfile.TemporaryDirectory() as tmp:
            for k, frame in enumerate(chosen):
                seconds = frame / episode['fps']
                out = Path(tmp) / f'{k}.png'
                subprocess.run(['ffmpeg', '-v', 'error', '-y', '-ss', str(seconds), '-i', str(video), '-frames:v', '1', str(out)], check=True)
                im = Image.open(out)
                if pov['kind'] == 'observer': im = im.crop((0, 0, im.width, round(im.height*596/720)))
                im.thumbnail((630, 315))
                x, y = (k % 3)*640 + 5, (k//3)*360 + 50
                draw.text((x, y), f't={seconds:.2f}s | frame {frame}', font=font, fill='#172a3a')
                sheet.paste(im, (x, y+30))
                provenance.append({'pov': name, 'frame': frame, 'seconds': seconds, 'source': str(video)})
        draw.text((10, 764), 'Observer: lower HUD cropped. Player: full frame. No changes to scene content.', font=font, fill='#536574')
        sheet.save(a.out / f'{name}-review.png')
    (a.out / 'review-samples.json').write_text(json.dumps(provenance, indent=2)+'\n')


if __name__ == '__main__': main()
