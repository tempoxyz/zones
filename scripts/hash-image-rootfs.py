#!/usr/bin/env python3
"""Hash the effective contents and execution-relevant metadata of a Docker export.

Read a `docker export` tar stream on stdin. Tar order and timestamps do not
affect the digest, while files, links, ownership, modes, devices, and xattrs do.
"""

import hashlib
import json
import sys
import tarfile


def main() -> None:
    entries = []
    names = set()
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as archive:
        for member in archive:
            if member.name in names:
                raise ValueError(f"duplicate path in Docker export: {member.name!r}")
            names.add(member.name)

            entry = {
                "path": member.name,
                "type": member.type.hex(),
                "mode": member.mode,
                "uid": member.uid,
                "gid": member.gid,
                "linkname": member.linkname,
                "devmajor": member.devmajor,
                "devminor": member.devminor,
                "pax": {
                    key: value
                    for key, value in member.pax_headers.items()
                    if key not in ("mtime", "atime", "ctime", "path", "linkpath")
                },
            }
            if member.isfile():
                file_hash = hashlib.sha256()
                source = archive.extractfile(member)
                if source is None:
                    raise ValueError(f"missing file data in Docker export: {member.name!r}")
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    file_hash.update(chunk)
                entry["size"] = member.size
                entry["sha256"] = file_hash.hexdigest()
            entries.append(entry)

    entries.sort(key=lambda entry: entry["path"])
    manifest = json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
    print(hashlib.sha256(manifest).hexdigest())


if __name__ == "__main__":
    try:
        main()
    except (OSError, tarfile.TarError, ValueError) as error:
        print(f"Unable to hash Docker export: {error}", file=sys.stderr)
        sys.exit(1)
