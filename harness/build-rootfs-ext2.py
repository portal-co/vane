#!/usr/bin/env python3
"""Build a minimal partitionless ext2 disk image (for the vane virtio-blk
disk-rootfs gate, PLAN_PI_IN_VM.md R8).

No mkfs/genext2fs is available offline, so this writes the ext2 structures
directly: 4096-byte blocks (so a busybox-sized /init fits in direct +
single-indirect blocks), one block group, an inode table, and directory data
blocks.

Usage: build-rootfs-ext2.py OUTPUT.img SIZE_SECTORS --file PATH:MODE:SRCFILE ...
"""
import struct
import sys

BLOCK = 4096


def build_image(files, total_sectors, out_path):
    total_blocks = (total_sectors * 512) // BLOCK
    n_inodes = 64

    # 4K-block layout: block 0 = boot + superblock (at byte 1024); block 1 =
    # block-group descriptor; block 2 = block bitmap; block 3 = inode bitmap;
    # blocks 4.. = inode table; then data blocks.
    inode_table_blocks = (n_inodes * 128 + BLOCK - 1) // BLOCK
    inode_table_start = 4
    first_data_block = inode_table_start + inode_table_blocks

    blocks = [bytearray(BLOCK) for _ in range(total_blocks)]
    block_bitmap = bytearray((total_blocks + 7) // 8)
    inode_bitmap = bytearray((n_inodes + 7) // 8)
    inodes = [bytearray(128) for _ in range(n_inodes)]

    used_blocks = set(range(first_data_block))
    dir_inodes = set()
    used_inodes = set()

    def alloc_block():
        for b in range(first_data_block, total_blocks):
            if b not in used_blocks:
                used_blocks.add(b)
                return b
        raise RuntimeError("out of blocks")

    def alloc_inode(is_dir):
        for i in range(11, n_inodes):
            if i not in used_inodes:
                used_inodes.add(i)
                if is_dir:
                    dir_inodes.add(i)
                return i
        raise RuntimeError("out of inodes")

    def write_inode(ino, mode, size, direct, single_indirect, links):
        d = inodes[ino - 1]
        struct.pack_into("<H", d, 0, mode)
        struct.pack_into("<I", d, 4, size)  # i_size (low)
        struct.pack_into("<H", d, 26, links)  # i_links_count
        n_sect = (len(direct) + (1 if single_indirect is not None else 0)) * (BLOCK // 512)
        struct.pack_into("<I", d, 28, n_sect)  # i_blocks (512-byte units)
        for i, bn in enumerate(direct):
            struct.pack_into("<I", d, 40 + i * 4, bn)  # i_block[0..11]
        if single_indirect is not None:
            struct.pack_into("<I", d, 40 + 12 * 4, single_indirect)  # i_block[12]

    # Build the path tree.
    root = {"is_dir": True, "mode": 0o40755, "children": {}}
    for path, mode, data in files:
        parts = [p for p in path.split("/") if p]
        node = root
        for i, p in enumerate(parts):
            if i == len(parts) - 1:
                node["children"][p] = {"is_dir": False, "mode": mode, "data": data}
            else:
                node = node["children"].setdefault(
                    p, {"is_dir": True, "mode": 0o40755, "children": {}}
                )

    used_inodes.add(2)
    dir_inodes.add(2)
    inode_of = {}

    def assign(node, path):
        ino = 2 if path == "/" else alloc_inode(node["is_dir"])
        inode_of[path] = ino
        if node["is_dir"]:
            for name, child in node["children"].items():
                assign(child, path.rstrip("/") + "/" + name)

    assign(root, "/")

    def parent_ino(path):
        p = path.rstrip("/")
        if not p.strip("/"):
            return 2
        return inode_of.get("/" + "/".join(p.strip("/").split("/")[:-1]), 2)

    def emit(node, path):
        ino = inode_of[path]
        if node["is_dir"]:
            entries = [(".", ino), ("..", parent_ino(path))]
            for name in node["children"]:
                entries.append((name, inode_of[path.rstrip("/") + "/" + name]))
            blk = alloc_block()
            data = bytearray(BLOCK)
            off = 0
            for name, cino in entries:
                nb = name.encode()
                rec_len = (8 + len(nb) + 3) & ~3
                struct.pack_into("<IHB", data, off, cino, rec_len, len(nb))
                is_dir = name in (".", "..") or node["children"].get(name, {}).get("is_dir", False)
                struct.pack_into("<B", data, off + 7, 2 if is_dir else 1)
                data[off + 8 : off + 8 + len(nb)] = nb
                off += rec_len
            blocks[blk][:] = data
            n_subdirs = sum(1 for c in node["children"].values() if c["is_dir"])
            write_inode(ino, node["mode"], BLOCK, [blk], None, links=2 + n_subdirs)
            for name, child in node["children"].items():
                emit(child, path.rstrip("/") + "/" + name)
        else:
            data = node["data"]
            nblocks = max(1, (len(data) + BLOCK - 1) // BLOCK)
            direct = []
            indirect = []
            for i in range(nblocks):
                b = alloc_block()
                blocks[b][:] = data[i * BLOCK : (i + 1) * BLOCK].ljust(BLOCK, b"\0")
                if len(direct) < 12:
                    direct.append(b)
                else:
                    indirect.append(b)
            single_indirect = None
            if indirect:
                single_indirect = alloc_block()
                si = bytearray(BLOCK)
                for i, bn in enumerate(indirect):
                    struct.pack_into("<I", si, i * 4, bn)
                blocks[single_indirect][:] = si
            write_inode(ino, node["mode"], len(data), direct, single_indirect, links=1)

    emit(root, "/")

    for b in used_blocks:
        block_bitmap[b // 8] |= 1 << (b % 8)
    for i in used_inodes:
        inode_bitmap[(i - 1) // 8] |= 1 << ((i - 1) % 8)
    blocks[2][: len(block_bitmap)] = block_bitmap
    blocks[3][: len(inode_bitmap)] = inode_bitmap

    free_blocks = total_blocks - len(used_blocks)
    free_inodes = n_inodes - len(used_inodes)

    bgdt = bytearray(BLOCK)
    struct.pack_into("<III", bgdt, 0, 2, 3, inode_table_start)
    struct.pack_into("<HHH", bgdt, 12, free_blocks, free_inodes, len(dir_inodes))
    blocks[1][:] = bgdt

    for i, idata in enumerate(inodes):
        blk = inode_table_start + (i * 128) // BLOCK
        off = (i * 128) % BLOCK
        blocks[blk][off : off + 128] = idata

    # Superblock: 1024 bytes, at byte offset 1024 (within block 0 for 4K blocks).
    sb = bytearray(1024)
    struct.pack_into("<IIII", sb, 0, len(used_inodes), total_blocks, 0, free_blocks)
    struct.pack_into("<I", sb, 16, free_inodes)
    struct.pack_into("<I", sb, 20, 0)  # s_first_data_block (0 for 4K blocks)
    struct.pack_into("<I", sb, 24, 2)  # s_log_block_size (1024 << 2 = 4096)
    struct.pack_into("<I", sb, 28, 2)  # s_log_frag_size
    struct.pack_into("<I", sb, 32, total_blocks)  # s_blocks_per_group
    struct.pack_into("<I", sb, 36, total_blocks)  # s_frags_per_group
    struct.pack_into("<I", sb, 40, n_inodes)  # s_inodes_per_group
    struct.pack_into("<H", sb, 56, 0xEF53)  # magic
    struct.pack_into("<HHH", sb, 58, 1, 1, 0)  # state, errors, minor_rev
    struct.pack_into("<IHH", sb, 84, 11, 128, 0)  # first_ino, inode_size, block_group_nr
    # Superblock lives at byte 1024 (block 0 for 4K blocks); exactly 1024 bytes.
    blocks[0][1024:2048] = sb

    try:
        with open(out_path, "wb") as f:
            for b in blocks:
                f.write(bytes(b))
    except OSError as e:
        raise RuntimeError(f"cannot write {out_path}: {e}") from e
    print(f"wrote {out_path}: {total_blocks} blocks (4K), {len(used_inodes)} inodes ({len(dir_inodes)} dirs), {free_blocks} free blocks")


def main():
    try:
        out = sys.argv[1]
        total_sectors = int(sys.argv[2])
        files = []
        i = 3
        while i < len(sys.argv):
            if sys.argv[i] == "--file":
                path, mode, src = sys.argv[i + 1].split(":", 2)
                with open(src, "rb") as f:
                    files.append((path, int(mode, 8), f.read()))
                i += 2
            else:
                i += 1
        build_image(files, total_sectors, out)
    except (OSError, ValueError, IndexError, RuntimeError) as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
