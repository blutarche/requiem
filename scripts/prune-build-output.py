#!/usr/bin/env python3
# Frees the disk that builds leave behind: Cargo target/ dirs of the main checkout and of its
# agent worktrees (.claude/worktrees/agent-*), agent worktrees that are finished, and the Docker
# volumes release-linux.sh leaves.
# Usage: scripts/prune-build-output.py [--apply] [--free-below GIB] [--repo PATH]
# Dry run unless --apply. Free space on the repo's filesystem below --free-below GiB is
# "pressure": targets go after 1 h idle instead of 24 h (main: 7 d), and the Linux build caches
# go too. launchd runs it hourly as `/usr/bin/python3 scripts/prune-build-output.py --apply
# --free-below 30`. Stdlib only, Python 3.9. It never forces anything: git's and Docker's own
# refusals keep a candidate.
import argparse
import errno
import fcntl
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime

GIB = 2 ** 30
HOUR = 3600
DAY = 24 * HOUR
PRESERVE = ("release-files", "linux")
VOLUME_RE = re.compile(r"^(requiem|rqm)-")
CACHE_VOLUMES = (
    "requiem-linux-cargo-registry",
    "requiem-linux-cargo-git",
    "requiem-linux-target-x86_64",
    "requiem-linux-target-aarch64",
)
SEARCH_PATH = os.pathsep.join(
    [os.environ.get("PATH", ""), "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/usr/sbin", "/bin", "/sbin"]
)
ENV = dict(os.environ, PATH=SEARCH_PATH)
DOCKER_UNITS = {"B": 1, "KB": 10 ** 3, "MB": 10 ** 6, "GB": 10 ** 9, "TB": 10 ** 12}


def which(name):
    return shutil.which(name, path=SEARCH_PATH)


def run(cmd, cwd=None, timeout=None):
    return subprocess.run(
        cmd, cwd=cwd, env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        universal_newlines=True, timeout=timeout,
    )


def fmt_size(n):
    if n is None:
        return "?"
    for unit, div in (("GiB", GIB), ("MiB", 2 ** 20), ("KiB", 2 ** 10)):
        if n >= div:
            return "%.1f %s" % (n / div, unit)
    return "%d B" % n


def fmt_idle(seconds):
    return "%.1fh" % (seconds / HOUR) if seconds < 2 * DAY else "%.1fd" % (seconds / DAY)


def tree_size(path, seen=None):
    """Allocated bytes under path, counting each hard-linked inode once."""
    seen = set() if seen is None else seen
    total = 0
    stack = [path]
    while stack:
        cur = stack.pop()
        try:
            st = os.lstat(cur)
            if (st.st_dev, st.st_ino) not in seen:
                seen.add((st.st_dev, st.st_ino))
                total += st.st_blocks * 512
            if os.path.isdir(cur) and not os.path.islink(cur):
                with os.scandir(cur) as it:
                    stack.extend(e.path for e in it)
        except OSError:
            pass
    return total


def delete_path(path):
    if os.path.isdir(path) and not os.path.islink(path):
        def chmod_retry(func, p, _exc):
            os.chmod(os.path.dirname(p), 0o700)
            func(p)
        shutil.rmtree(path, onerror=chmod_retry)
    else:
        os.unlink(path)


class Ctx:
    def __init__(self, apply, free_below, main, git):
        self.apply = apply
        self.free_below = None if free_below is None else int(free_below * GIB)
        self.main = main
        self.git = git
        self.now = time.time()
        self.reclaim = 0
        self.warned = set()
        self.cwds = None
        self.pressure = self.free_below is not None and self.free() < self.free_below

    def free(self):
        return shutil.disk_usage(self.main).free

    def relieved(self):
        return self.apply and self.pressure and self.free() >= self.free_below

    def log(self, action, subject, size=None, reason=""):
        parts = ["%-16s %s" % (action, subject)]
        if size is not None:
            parts.append(fmt_size(size))
        if reason:
            parts.append(reason)
        print("  ".join(parts), flush=True)

    def warn(self, key, message):
        if key not in self.warned:
            self.warned.add(key)
            print("WARN             " + message, flush=True)

    def run_git(self, *args, **kw):
        return run([self.git] + list(args), cwd=kw.get("cwd", self.main))


def resolve_main(git, directory):
    r = run([git, "rev-parse", "--git-common-dir"], cwd=directory)
    if r.returncode != 0:
        raise RuntimeError("not a git repository: %s: %s" % (directory, r.stderr.strip()))
    common = os.path.realpath(os.path.join(directory, r.stdout.strip()))
    return os.path.dirname(common)


def list_worktrees(ctx):
    r = ctx.run_git("worktree", "list", "--porcelain")
    if r.returncode != 0:
        raise RuntimeError("git worktree list failed: " + r.stderr.strip())
    root = os.path.join(ctx.main, ".claude", "worktrees") + os.sep
    found = []
    for block in r.stdout.split("\n\n"):
        d = {}
        for line in block.splitlines():
            key, _, value = line.partition(" ")
            d[key] = value
        if "worktree" not in d:
            continue
        path = os.path.realpath(d["worktree"])
        if not (path + os.sep).startswith(root) or path + os.sep == root:
            continue
        branch = d.get("branch", "")
        found.append({
            "path": path,
            "branch": branch[len("refs/heads/"):] if branch.startswith("refs/heads/") else None,
            "detached": "detached" in d,
            "locked": "locked" in d,
            "prunable": "prunable" in d,
        })
    return found


def live_cwds(ctx):
    """Working directories of running cargo and rustc processes, or None when they can't be read."""
    pids = []
    for name in ("cargo", "rustc"):
        try:
            r = run(["pgrep", "-x", name])
        except OSError as e:
            ctx.warn("pgrep", "cannot run pgrep (%s); relying on the Cargo lock check" % e)
            return None
        if r.returncode > 1:
            ctx.warn("pgrep", "pgrep failed (%s); relying on the Cargo lock check" % r.stderr.strip())
            return None
        pids.extend(r.stdout.split())
    if not pids:
        return []
    try:
        r = run(["lsof", "-a", "-p", ",".join(pids), "-d", "cwd", "-Fn"], timeout=30)
    except (OSError, subprocess.TimeoutExpired) as e:
        ctx.warn("lsof", "cannot run lsof (%s); relying on the Cargo lock check" % e)
        return None
    return [os.path.realpath(line[1:]) for line in r.stdout.splitlines() if line.startswith("n")]


def cwd_inside(ctx, root, others):
    """True when a live cargo/rustc runs in root and not in a nested checkout in others."""
    for cwd in ctx.cwds or []:
        if (cwd == root or cwd.startswith(root + os.sep)) and not any(
            cwd == o or cwd.startswith(o + os.sep) for o in others
        ):
            return True
    return False


def cargo_dirs(target):
    """Cargo's per-profile dirs: target/<profile> and, for cross builds, target/<triple>/<profile>."""
    dirs = []
    try:
        tops = [e for e in os.scandir(target) if e.is_dir(follow_symlinks=False) and e.name not in PRESERVE]
    except OSError:
        return dirs
    for top in tops:
        dirs.append(top.path)
        try:
            dirs.extend(e.path for e in os.scandir(top.path) if e.is_dir(follow_symlinks=False))
        except OSError:
            pass
    return dirs


def lock_held(target):
    for d in cargo_dirs(target):
        lock = os.path.join(d, ".cargo-lock")
        try:
            fd = os.open(lock, os.O_RDONLY)
        except OSError:
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fd, fcntl.LOCK_UN)
        except OSError as e:
            if e.errno in (errno.EWOULDBLOCK, errno.EAGAIN):
                return True
        finally:
            os.close(fd)
    return False


def target_recency(target):
    """Newest build activity under target/, or None when there is no Cargo state to read.

    Reads each profile dir's .cargo-lock and the entries directly inside its .fingerprint and
    incremental dirs. A target/ with none of those falls back to its other top-level children.
    """
    newest = None

    def note(st):
        nonlocal newest
        newest = st.st_mtime if newest is None else max(newest, st.st_mtime)

    for d in cargo_dirs(target):
        try:
            note(os.stat(os.path.join(d, ".cargo-lock")))
        except OSError:
            pass
        for sub in (".fingerprint", "incremental"):
            try:
                with os.scandir(os.path.join(d, sub)) as it:
                    for e in it:
                        note(e.stat(follow_symlinks=False))
            except OSError:
                pass
    if newest is None:
        try:
            for e in os.scandir(target):
                if e.name not in PRESERVE:
                    note(e.stat(follow_symlinks=False))
        except OSError:
            pass
    return newest


def source_recency(root):
    """Newest mtime of anything in a worktree outside target/ and .git."""
    newest = 0.0
    for cur, dirs, files in os.walk(root):
        if cur == root:
            dirs[:] = [d for d in dirs if d not in ("target", ".git")]
            files = [f for f in files if f != ".git"]
        for name in dirs + files + [""]:
            try:
                newest = max(newest, os.lstat(os.path.join(cur, name) if name else cur).st_mtime)
            except OSError:
                pass
    return newest or None


def live_reason(ctx, root, target, others):
    if os.path.isdir(target) and lock_held(target):
        return "live build (lock held)"
    if cwd_inside(ctx, root, others):
        return "live build (cargo or rustc running in the checkout)"
    return None


def prune_target(ctx, root, target, idle, live, threshold):
    if not os.path.isdir(target):
        ctx.log("KEEP", target, reason="no target dir")
        return
    kids = sorted(e.path for e in os.scandir(target) if e.name not in PRESERVE)
    if not kids:
        ctx.log("KEEP", target, reason="nothing to prune")
        return
    seen = set()
    sizes = [(k, tree_size(k, seen)) for k in kids]
    total = sum(s for _, s in sizes)
    if live:
        ctx.log("SKIP", target, total, live)
        return
    label = "idle %s" % ("unknown" if idle is None else fmt_idle(idle))
    if idle is None or idle <= threshold:
        ctx.log("KEEP", target, total, "%s, under the %s limit" % (label, fmt_idle(threshold)))
        return
    if ctx.relieved():
        ctx.log("KEEP", target, total, "free space is back above %s GiB" % (ctx.free_below // GIB))
        return
    if not ctx.apply:
        ctx.reclaim += total
        ctx.log("DELETE", target, total, "%s, over the %s limit" % (label, fmt_idle(threshold)))
        return
    freed = 0
    try:
        for kid, size in sizes:
            delete_path(kid)
            freed += size
    except OSError as e:
        ctx.log("SKIP", target, total, "delete failed: %s" % e)
    else:
        ctx.log("DELETE", target, total, "%s, over the %s limit" % (label, fmt_idle(threshold)))
    ctx.reclaim += freed


def unmerged_reason(ctx, wt):
    ref = "refs/heads/" + wt["branch"]
    if ctx.run_git("merge-base", "--is-ancestor", ref, "refs/heads/main").returncode == 0:
        return None
    remote = ctx.run_git("rev-parse", "--verify", "-q", "refs/remotes/origin/" + wt["branch"])
    local = ctx.run_git("rev-parse", "--verify", "-q", ref)
    if remote.returncode == 0 and local.returncode == 0 and remote.stdout.strip() == local.stdout.strip():
        return None
    return "branch is not merged into main or pushed unchanged to origin"


def removal_blockers(ctx, wt, idle, live):
    blockers = []
    if wt["locked"]:
        blockers.append("locked")
    if wt["prunable"]:
        blockers.append("prunable")
    if wt["detached"] or wt["branch"] is None:
        blockers.append("detached HEAD")
    if blockers and not os.path.isdir(wt["path"]):
        return blockers
    st = ctx.run_git("status", "--porcelain", cwd=wt["path"])
    if st.returncode != 0:
        blockers.append("git status failed: " + st.stderr.strip())
    elif st.stdout.strip():
        blockers.append("uncommitted changes")
    if idle is None or idle <= HOUR:
        blockers.append("idle %s, under 1.0h" % ("unknown" if idle is None else fmt_idle(idle)))
    if live:
        blockers.append(live)
    if wt["branch"] is not None:
        reason = unmerged_reason(ctx, wt)
        if reason:
            blockers.append(reason)
    return blockers


def first_line(text):
    lines = text.strip().splitlines()
    return lines[0] if lines else "no message"


def process_worktree(ctx, wt, others):
    path = wt["path"]
    target = os.path.join(path, "target")
    if not os.path.isdir(path):
        ctx.log("KEEP", path, reason="prunable: directory is missing")
        return
    recent = [t for t in (target_recency(target) if os.path.isdir(target) else None, source_recency(path)) if t]
    idle = ctx.now - max(recent) if recent else None
    live = live_reason(ctx, path, target, others)
    blockers = removal_blockers(ctx, wt, idle, live)
    if not blockers:
        size = tree_size(path)
        if not ctx.apply:
            ctx.reclaim += size
            ctx.log("REMOVE-WORKTREE", path, size, "branch %s is merged and the tree is clean and idle; branch -d follows" % wt["branch"])
            return
        r = ctx.run_git("worktree", "remove", path)
        if r.returncode == 0:
            ctx.reclaim += size
            ctx.log("REMOVE-WORKTREE", path, size, "branch %s is merged and the tree is clean and idle" % wt["branch"])
            b = ctx.run_git("branch", "-d", wt["branch"])
            if b.returncode == 0:
                ctx.log("DELETE", "branch " + wt["branch"], reason="merged; worktree removed")
            else:
                ctx.log("KEEP", "branch " + wt["branch"], reason="git branch -d refused: " + first_line(b.stderr))
            return
        blockers = ["git worktree remove refused: " + first_line(r.stderr)]
    ctx.log("KEEP", path, reason="; ".join(blockers))
    prune_target(ctx, path, target, idle, live, HOUR if ctx.pressure else DAY)


def prune_main_target(ctx, others):
    target = os.path.join(ctx.main, "target")
    newest = target_recency(target) if os.path.isdir(target) else None
    idle = None if newest is None else ctx.now - newest
    live = live_reason(ctx, ctx.main, target, others)
    prune_target(ctx, ctx.main, target, idle, live, HOUR if ctx.pressure else 7 * DAY)


def parse_docker_size(text):
    m = re.match(r"^([\d.]+)\s*([kKMGT]?B)$", text)
    if not m:
        return None
    unit = m.group(2).upper()
    return int(float(m.group(1)) * DOCKER_UNITS[unit])


def docker_volume_sizes(docker):
    try:
        r = run([docker, "system", "df", "-v"], timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return {}
    sizes = {}
    in_section = False
    for line in r.stdout.splitlines():
        if line.startswith("Local Volumes space usage"):
            in_section = True
        elif in_section and not line.strip():
            if sizes:
                break
        elif in_section and not line.startswith("VOLUME NAME"):
            fields = line.split()
            if len(fields) >= 3:
                sizes[fields[0]] = parse_docker_size(fields[-1])
    return sizes


def prune_volumes(ctx):
    docker = which("docker")
    if not docker:
        ctx.log("SKIP", "docker", reason="docker unavailable (not found)")
        return
    try:
        info = run([docker, "info"], timeout=10)
    except (OSError, subprocess.TimeoutExpired) as e:
        ctx.log("SKIP", "docker", reason="docker unavailable (%s)" % e)
        return
    if info.returncode != 0:
        ctx.log("SKIP", "docker", reason="docker unavailable (%s)" % first_line(info.stderr))
        return
    ls = run([docker, "volume", "ls", "--format", "{{.Name}}"], timeout=30)
    ps = run([docker, "ps", "-a", "-q"], timeout=30)
    if ls.returncode != 0 or ps.returncode != 0:
        ctx.log("SKIP", "docker", reason="docker unavailable (cannot list volumes or containers)")
        return
    used = set()
    ids = ps.stdout.split()
    if ids:
        insp = run([docker, "inspect", "-f", "{{range .Mounts}}{{.Name}}\n{{end}}"] + ids, timeout=60)
        if insp.returncode != 0:
            ctx.log("SKIP", "docker", reason="cannot inspect containers: " + first_line(insp.stderr))
            return
        used = set(insp.stdout.split())
    names = sorted(n for n in ls.stdout.split() if VOLUME_RE.match(n))
    sizes = docker_volume_sizes(docker) if names else {}
    for name in names:
        size = sizes.get(name)
        subject = "volume " + name
        if name in used:
            ctx.log("KEEP", subject, size, "referenced by a container")
        elif name in CACHE_VOLUMES and not ctx.pressure:
            ctx.log("KEEP", subject, size, "live build cache; removed only under disk pressure")
        elif name in CACHE_VOLUMES and ctx.relieved():
            ctx.log("KEEP", subject, size, "free space is back above %s GiB" % (ctx.free_below // GIB))
        elif not ctx.apply:
            ctx.reclaim += size or 0
            ctx.log("DELETE", subject, size, "unreferenced" if name not in CACHE_VOLUMES else "unreferenced build cache, disk pressure")
        else:
            r = run([docker, "volume", "rm", name], timeout=120)
            if r.returncode == 0:
                ctx.reclaim += size or 0
                ctx.log("DELETE", subject, size, "unreferenced")
            else:
                ctx.log("KEEP", subject, size, "docker volume rm refused: " + first_line(r.stderr))


def main():
    ap = argparse.ArgumentParser(description="Prune Cargo target dirs, finished agent worktrees and Docker build volumes.")
    ap.add_argument("--apply", action="store_true", help="delete; without it, only report")
    ap.add_argument("--free-below", type=float, metavar="GIB", help="prune harder when free space is below this")
    ap.add_argument("--repo", metavar="PATH", help="repo to prune (default: the checkout this script is in)")
    args = ap.parse_args()

    git = which("git")
    if not git:
        raise RuntimeError("git not found")
    repo = args.repo
    if repo is None:
        script_dir = os.path.dirname(os.path.realpath(__file__))
        top = run([git, "rev-parse", "--show-toplevel"], cwd=script_dir)
        if top.returncode != 0:
            raise RuntimeError("not inside a git repository: " + top.stderr.strip())
        repo = top.stdout.strip()
    ctx = Ctx(args.apply, args.free_below, resolve_main(git, repo), git)

    free_before = ctx.free()
    print("%s  %s, pressure %s, free %s" % (
        datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "apply" if ctx.apply else "dry run",
        "on" if ctx.pressure else "off",
        fmt_size(free_before),
    ), flush=True)

    ctx.cwds = live_cwds(ctx)
    worktrees = list_worktrees(ctx)
    paths = [w["path"] for w in worktrees]
    registered = set(paths)
    wt_root = os.path.join(ctx.main, ".claude", "worktrees")
    if os.path.isdir(wt_root):
        for name in sorted(os.listdir(wt_root)):
            full = os.path.realpath(os.path.join(wt_root, name))
            if os.path.isdir(full) and full not in registered:
                ctx.log("SKIP", full, reason="unregistered dir")

    def idle_of(wt):
        recent = [t for t in (target_recency(os.path.join(wt["path"], "target")), source_recency(wt["path"])) if t]
        return ctx.now - max(recent) if recent else 0

    # Longest idle first, so a pressure run frees the stalest targets before it has to stop.
    for wt in sorted(worktrees, key=lambda w: (-idle_of(w), w["path"]) if os.path.isdir(w["path"]) else (0, w["path"])):
        process_worktree(ctx, wt, [p for p in paths if p != wt["path"]])
    prune_main_target(ctx, paths)
    prune_volumes(ctx)

    free_after = ctx.free()
    print("SUMMARY          %s %s; free %s -> %s" % (
        "reclaimed" if ctx.apply else "would reclaim",
        fmt_size(ctx.reclaim),
        fmt_size(free_before),
        fmt_size(free_after),
    ), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001
        print("prune-build-output.py: %s" % e, file=sys.stderr)
        sys.exit(1)
