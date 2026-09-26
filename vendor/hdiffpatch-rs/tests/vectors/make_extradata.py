#!/usr/bin/env python3
"""
Synthesises an HDIFFW26 vector with extraDataSize > 0.

The hdiffz CLI never emits extraData, so the only way to cover that branch is to
build one. Takes an uncompressed, unchecksummed W26 patch, rewrites the head with
a non-zero extraDataSize, and inserts that many filler bytes at the start of the
window body. Requires comp=none (body is plain) and checksum=none (no digest to
invalidate).

    ./make_extradata.py <in.hdiff> <out.hdiff> [nbytes]

Validate the result with upstream `hpatchz` before trusting it as a test vector.
"""
import sys

MAGIC = b'HDIFFW26'
PREFIX = 10


def unpack(buf, i):
    b = buf[i]
    i += 1
    v = b & 0x7F
    if b & 0x80:
        while True:
            b = buf[i]
            i += 1
            v = (v << 7) | (b & 0x7F)
            if not (b & 0x80):
                break
    return v, i


def pack(v):
    groups = [v & 0x7F]
    v >>= 7
    while v:
        groups.append(v & 0x7F)
        v >>= 7
    groups.reverse()
    return bytes(g | (0x80 if k < len(groups) - 1 else 0) for k, g in enumerate(groups))


def main():
    src, dst = sys.argv[1], sys.argv[2]
    n = int(sys.argv[3]) if len(sys.argv) > 3 else 777

    d = open(src, 'rb').read()
    assert d[:8] == MAGIC, 'not an HDIFFW26 file'
    head_remaining = d[8] | (d[9] << 8)
    head = d[PREFIX:PREFIX + head_remaining]
    body = d[PREFIX + head_remaining:]

    amp = head.index(b'&')
    nul = head.index(b'\0', amp + 1)
    comp_type = head[:amp]
    checksum_type = head[amp + 1:nul]
    assert comp_type == b'', 'need an uncompressed patch (-c-no)'
    assert checksum_type == b'', 'need a checksum-free patch (-C-no)'

    i = nul + 1
    fields = []
    for _ in range(12):
        v, i = unpack(head, i)
        fields.append(v)
    trailing = head[i:]

    (compressed, uncompressed, new_size, old_size, cover_count, window_count,
     meta_count, max_step, max_sub, max_win_old, checksum_bytes, extra) = fields
    assert compressed == 0, 'compressedSize must be 0'
    assert extra == 0, 'extraDataSize is already non-zero'

    fields[1] = uncompressed + n
    fields[11] = n

    new_head = comp_type + b'&' + checksum_type + b'\0' + b''.join(pack(v) for v in fields) + trailing
    assert len(new_head) < 4096 - PREFIX, 'head exceeds hpatch_kWindowDiffHeadMaxSize'

    out = MAGIC + bytes([len(new_head) & 0xFF, (len(new_head) >> 8) & 0xFF]) + new_head
    out += bytes((k * 37 + 11) & 0xFF for k in range(n)) + body
    open(dst, 'wb').write(out)
    print('wrote %s: extraDataSize=%d uncompressedSize=%d' % (dst, n, fields[1]))


if __name__ == '__main__':
    main()
