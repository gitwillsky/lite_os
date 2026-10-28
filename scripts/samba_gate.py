#!/usr/bin/env python3
"""固定上游 Samba 的非 root 签名/加密验收环境；只在显式运行时启动服务。"""
from __future__ import annotations
import argparse
import pwd
import platform
import hashlib
import json
import os
import secrets
import shutil
import signal
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.request
from pathlib import Path
from local_inputs import INPUTS, TOOLS
from build_cache import cache_lock

VERSION = "4.22.11"
SHA256 = "d569b298a81cc212a39e9d1813bc4ca5c71ac93c4ceb2eec03db178096958d00"
URL = f"https://download.samba.org/pub/samba/stable/samba-{VERSION}.tar.gz"
PREFIX = TOOLS / "samba" / VERSION
CONFIGURE = ("--without-ad-dc", "--without-ads", "--without-ldap", "--disable-python", "--without-pam", "--disable-cups", "--without-libarchive", "--without-json", "--without-acl-support")

def build() -> Path:
    """校验固定源码并构建 standalone smbd/pdbedit 到 .local；不启动或执行验收。"""
    base = INPUTS / "samba"
    base.mkdir(parents=True, exist_ok=True)
    archive = base / f"samba-{VERSION}.tar.gz"
    with cache_lock(base / ".build.lock"):
        if not archive.exists():
            temporary = archive.with_suffix(".download")
            with urllib.request.urlopen(URL, timeout=60) as response, temporary.open("wb") as target:
                shutil.copyfileobj(response, target)
            temporary.replace(archive)
        if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
            raise RuntimeError("Samba source digest mismatch")
        identity = {"version": VERSION, "sha256": SHA256, "configure": list(CONFIGURE), "host": platform.system() + "-" + platform.machine(), "recipe": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        stamp = PREFIX / "liteos-build.json"
        if stamp.is_file() and json.loads(stamp.read_text()) == identity and (PREFIX / "sbin/smbd").is_file() and (PREFIX / "bin/pdbedit").is_file():
            return PREFIX
        source = base / "source" / f"samba-{VERSION}"
        if not source.exists():
            with tarfile.open(archive) as stream:
                stream.extractall(source.parent, filter="data")
        subprocess.run([str(source / "configure"), f"--prefix={PREFIX}", *CONFIGURE], cwd=source, check=True)
        subprocess.run(["make", f"-j{min(os.cpu_count() or 1, 8)}"], cwd=source, check=True)
        subprocess.run(["make", "install"], cwd=source, check=True)
        stamp.write_text(json.dumps(identity, indent=2) + "\n")
    return PREFIX

def configuration(work: Path, port: int, policy: str, user: str, bind: str) -> Path:
    """生成仅当前 Unix 用户可认证的 SMB2.1/3 共享；状态、日志与凭据均在私有临时目录。"""
    for name in ("private", "lock", "state", "cache", "pid", "share"):
        (work / name).mkdir(mode=0o700)
    path = work / "smb.conf"
    path.write_text(f"""[global]
server role = standalone server
workgroup = WORKGROUP
netbios name = LITEOSGATE
security = user
map to guest = Never
server min protocol = SMB2_10
server max protocol = SMB3_11
server signing = mandatory
smb ports = {port}
interfaces = {bind}
bind interfaces only = yes
disable netbios = yes
load printers = no
printing = bsd
printcap name = /dev/null
ntlm auth = ntlmv2-only
passdb backend = tdbsam:{work}/private/passdb.tdb
private dir = {work}/private
lock directory = {work}/lock
state directory = {work}/state
cache directory = {work}/cache
pid directory = {work}/pid
log file = {work}/smbd.log
[share]
path = {work}/share
read only = no
valid users = {user}
force user = {user}
smb encrypt = {'required' if policy == 'encrypt' else 'off'}
""")
    return path

def serve(args) -> None:
    """启动并持有自己的 smbd 子进程；退出时只停止该子进程并移除私有凭据。"""
    if os.geteuid() == 0:
        raise RuntimeError("run the Samba gate as an ordinary host user")
    prefix = build()
    root = INPUTS / "samba/runs"
    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f"{args.policy}-", dir=root) as temporary:
        work = Path(temporary)
        user = pwd.getpwuid(os.geteuid()).pw_name
        password = secrets.token_urlsafe(24)
        config = configuration(work, args.port, args.policy, user, args.bind)
        # pdbedit 直接读 stdin，不把密码放进 argv/env；非 root 模式只创建当前 Unix 用户的私有 passdb。
        subprocess.run([str(prefix / "bin/pdbedit"), "-s", str(config), "-a", "-t", "-u", user], input=f"{password}\n{password}\n", text=True, check=True)
        credentials = work / "credentials"
        with credentials.open("x", opener=lambda path, flags: os.open(path, flags, 0o600)) as output:
            output.write(f"username={user}\npassword={password}\ndomain=WORKGROUP\n")
        del password
        with (work / "console.log").open("wb") as log:
            child = subprocess.Popen([str(prefix / "sbin/smbd"), "-F", "--no-process-group", "-s", str(config)], stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
            try:
                deadline = time.monotonic() + 20
                while True:
                    if child.poll() is not None:
                        raise RuntimeError(f"smbd exited; inspect {work / 'console.log'}")
                    try:
                        with socket.create_connection((args.connect, args.port), timeout=0.5):
                            break
                    except OSError:
                        if time.monotonic() > deadline:
                            raise RuntimeError(f"smbd did not listen; inspect {work / 'console.log'}")
                        time.sleep(0.1)
                print(json.dumps({"policy": args.policy, "port": args.port, "credentials": str(credentials), "share": str(work / "share"), "guest_source": f"//{args.guest_host}:{args.port}/share"}), flush=True)
                while child.poll() is None:
                    time.sleep(0.5)
            finally:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    child.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()

def interrupted(_signal, _frame) -> None:
    raise KeyboardInterrupt

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-only", action="store_true")
    parser.add_argument("--policy", choices=("sign", "encrypt"), default="sign")
    parser.add_argument("--port", type=int, default=1445)
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--connect", default="127.0.0.1")
    parser.add_argument("--guest-host", default="10.0.2.2")
    args = parser.parse_args()
    if not 1024 <= args.port <= 65535:
        parser.error("choose an unprivileged TCP port")
    signal.signal(signal.SIGTERM, interrupted)
    if args.build_only:
        print(build())
    else:
        serve(args)

if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
