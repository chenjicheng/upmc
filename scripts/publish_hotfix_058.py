"""One-time, explicitly requested 0.5.8 publication without CI test gates."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def run(*args):
    subprocess.run(args, check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--artifact', required=True, type=Path)
    parser.add_argument('--pages', required=True, type=Path)
    parser.add_argument('--build-id', required=True)
    args = parser.parse_args()
    artifact = args.artifact.read_bytes()
    descriptor = {
        'version': '0.5.8',
        'build_id': args.build_id,
        'download_url': 'https://gh.chenjicheng.cn/https://github.com/chenjicheng/upmc/releases/download/v0.5.8/updater.exe',
        'sha256': hashlib.sha256(artifact).hexdigest(),
        'size': len(artifact),
    }
    run('gh', 'release', 'create', 'v0.5.8', str(args.artifact),
        '--repo', 'chenjicheng/upmc', '--target', args.build_id,
        '--title', 'v0.5.8', '--notes',
        '自动下载 Java 21 到整合包 updater/java-21 并供 Fabric、Packwiz 优先使用，'
        '启动 PCL 时传入整合包 Java 环境；统一 GitHub 下载代理。'
        '\n\n应维护者要求直接构建发布，本次未运行测试或独立审查。')
    paths = ['bridge/version.json', 'bridge/dev/version.json', '.upmc-release-0.5.8.json']
    for name in paths:
        path = args.pages / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(descriptor, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    for name in ['version.json', 'dev/version.json', '.upmc-legacy-transition.json']:
        path = args.pages / name
        value = json.loads(path.read_text(encoding='utf-8'))
        if value['download_url'].startswith('https://github.com/'):
            value['download_url'] = 'https://gh.chenjicheng.cn/' + value['download_url']
            path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
            paths.append(name)
    run('git', '-C', str(args.pages), 'add', '--', *paths)
    run('git', '-C', str(args.pages), 'commit', '-m', 'Publish 0.5.8 with managed Java and proxied downloads')
    run('git', '-C', str(args.pages), 'push', 'origin', 'HEAD:gh-pages')


if __name__ == '__main__':
    main()
