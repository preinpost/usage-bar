#!/usr/bin/env python3
"""Bump the usage-bar version, commit, tag and push (GitHub Actions release bot).

Usage: bump.py [explicit_version] [patch|minor|major] [branch]
  - explicit_version ('' = auto): e.g. "1.2.3" or "v1.2.3"
  - bump level used when version is empty
  - branch to push the bump commit to (the workflow passes github.ref_name)

Git steps are skipped when GITHUB_TOKEN is not set, so local runs just print
the new version (dry run, no files touched).
"""
import os
import re
import subprocess
import sys

VERSION_RE = re.compile(r'^version\s*=\s*"([^"]+)"', re.M)
LOCK_RE = re.compile(r'(^name = "usage-bar"\nversion = ")[^"]+(")', re.M)


def read_version():
    text = open('Cargo.toml').read()
    m = VERSION_RE.search(text)
    if not m:
        sys.exit('no version field in Cargo.toml')
    return m.group(1)


def write_version(v):
    text = VERSION_RE.sub(f'version = "{v}"', open('Cargo.toml').read(), count=1)
    open('Cargo.toml', 'w').write(text)
    # keep the root package entry in Cargo.lock in sync so --locked builds pass
    lock = open('Cargo.lock').read()
    if 'name = "usage-bar"' in lock:
        open('Cargo.lock', 'w').write(LOCK_RE.sub(rf'\g<1>{v}\g<2>', lock, count=1))


def bump(v, kind):
    major, minor, patch = (int(x) for x in v.split('.'))
    if kind == 'major':
        return f'{major + 1}.0.0'
    if kind == 'minor':
        return f'{major}.{minor + 1}.0'
    return f'{major}.{minor}.{patch + 1}'


def main():
    explicit = sys.argv[1].strip().lstrip('v') if len(sys.argv) > 1 else ''
    kind = sys.argv[2] if len(sys.argv) > 2 else 'patch'
    branch = sys.argv[3] if len(sys.argv) > 3 else ''

    if explicit:
        if not re.fullmatch(r'\d+\.\d+\.\d+', explicit):
            sys.exit(f'bad explicit version: {explicit!r} (want X.Y.Z)')
        new = explicit
    else:
        new = bump(read_version(), kind)

    token = os.environ.get('GITHUB_TOKEN', '')
    if not token:
        print(new, flush=True)  # dry run — no files touched
        return

    write_version(new)
    print(new, flush=True)

    repo = os.environ['GITHUB_REPOSITORY']
    url = f'https://x-access-token:{token}@github.com/{repo}.git'
    # git talks on stdout (e.g. `[master daf883d] release: v0.1.1`); keep the
    # captured script output to exactly the version number, so GITHUB_OUTPUT
    # never sees the commit summary
    changed = bool(sh('git status --porcelain').strip())
    if changed:
        run(['git', 'add', 'Cargo.toml', 'Cargo.lock'])
        run(['git', '-c', 'user.name=usage-bar bot',
             '-c', 'user.email=actions@users.noreply.github.com',
             'commit', '-m', f'release: v{new}'])
    # idempotent: re-runs with the same version keep the existing tag
    if sh(f'git rev-parse -q --verify refs/tags/v{new}') == '':
        run(['git', 'tag', f'v{new}'])
    if branch:
        run(['git', 'push', url, f'HEAD:refs/heads/{branch}'])
    run(['git', 'push', url, f'v{new}'])


def sh(argv):
    return subprocess.run(argv, capture_output=True, text=True).stdout


def run(argv):
    # stdout stays quiet (git's commit summary etc.); errors surface on failure
    r = subprocess.run(argv, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f'{" ".join(argv)} failed:\n{r.stderr}')


if __name__ == '__main__':
    main()
