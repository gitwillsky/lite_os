"""产品文件安装契约；构建、同步与运行库装配共用同一值类型。"""
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class UserlandArtifact:
    """一个安装到开发/rootfs 镜像的系统文件。

    Attributes:
        source: 已构建并校验的 host 文件。
        destination: 文件在 guest 中的绝对路径。
        mode: regular file 的 Unix permission bits。
    """

    source: Path
    destination: str
    mode: int = 0o644
