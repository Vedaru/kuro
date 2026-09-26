#!/usr/bin/env bash
#
# Generates the round-trip test corpus using the upstream HDiffPatch tool.
#
#   ./tests/vectors/generate.sh <output-dir> [path-to-hdiffz]
#
# Every vector is produced by the reference implementation, so the tests compare
# this crate against upstream rather than against itself.
#
# Building hdiffz (it expects four sibling forks next to the checkout):
#
#   git clone --depth 1 https://github.com/sisong/HDiffPatch.git
#   git clone --depth 1 https://github.com/sisong/zstd.git
#   git clone --depth 1 -b bit_pos_padding https://github.com/sisong/zlib.git
#   git clone --depth 1 https://github.com/sisong/bzip2.git
#   git clone --depth 1 https://github.com/sisong/lzma.git
#   cd HDiffPatch && make -j8 LDEF=0 MD5=0 VCD=0 BSD=0 MT=0 \
#                             ZSTD=1 ZLIB=1 BZIP2=1 LZMA=1
#
# Output: <output-dir>/manifest.tsv plus the data files it references.
# manifest columns: name  kind  format  comp  checksum  windows  old  new
set -uo pipefail

OUT=${1:?usage: generate.sh <output-dir> [hdiffz]}
HDIFFZ=${2:-hdiffz}
command -v "$HDIFFZ" >/dev/null 2>&1 || [ -x "$HDIFFZ" ] || { echo "hdiffz not found: $HDIFFZ" >&2; exit 1; }

mkdir -p "$OUT"/data
MAN="$OUT/manifest.tsv"
: > "$MAN"

python3 - "$OUT" <<'PY'
import os, random, sys
out = sys.argv[1]
d = os.path.join(out, 'data')

def w(name, b): open(os.path.join(d, name), 'wb').write(b)
def rnd(n, seed):
    r = random.Random(seed)
    return bytes(r.randrange(256) for _ in range(n))

def edit(src, seed, n_edits=30):
    r = random.Random(seed)
    b = bytearray(src)
    for _ in range(n_edits):
        if len(b) < 64: break
        p = r.randrange(0, max(1, len(b) - 40))
        op = r.randrange(3)
        if op == 0: b[p:p+24] = bytes(r.randrange(256) for _ in range(24))
        elif op == 1: b[p:p] = bytes(r.randrange(256) for _ in range(r.randrange(1, 400)))
        else: del b[p:p + r.randrange(1, min(120, len(b) - p))]
    return bytes(b)

base = rnd(4 << 20, 1)
w('old.bin', base)
w('new.bin', edit(base, 2, 200) + rnd(50000, 3))

w('empty_old.bin', b'')
w('empty_new.bin', rnd(5000, 10))
w('one_old.bin', b'\x5a')
w('one_new.bin', b'\x5a\x99')
w('same_old.bin', rnd(100000, 11))
w('same_new.bin', open(os.path.join(d, 'same_old.bin'), 'rb').read())
w('shrink_old.bin', rnd(200000, 12))
w('shrink_new.bin', b'')

for count in (16, 31, 32, 33, 63, 64, 65, 96, 129):
    o = rnd(count * 65536, 100 + count)
    w('win%d_old.bin' % count, o)
    w('win%d_new.bin' % count, edit(o, 200 + count, 40))
PY

build_dirs() {
    local root="$OUT/data/$1"; shift
    rm -rf "$root"; mkdir -p "$root/old" "$root/new"
    python3 - "$root" <<'PY'
import os, random, sys
root = sys.argv[1]
r = random.Random(77)
def rnd(n): return bytes(r.randrange(256) for _ in range(n))

def put(side, name, data):
    p = os.path.join(root, side, name)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    open(p, 'wb').write(data)

for name, size in (('a.bin', 300000), ('sub/b.bin', 400000), ('sub/deep/c.dat', 150000)):
    o = rnd(size)
    put('old', name, o)
    b = bytearray(o)
    for _ in range(15):
        p = r.randrange(0, len(b) - 500)
        b[p:p+200] = rnd(200)
    put('new', name, bytes(b))

same = rnd(120000)
put('old', 'identical.bin', same)
put('new', 'identical.bin', same)
put('old', 'sub/identical2.dat', same[:40000])
put('new', 'sub/identical2.dat', same[:40000])

put('new', 'added.bin', rnd(70000))
put('old', 'removed.bin', rnd(50000))
put('old', 'empty_both.bin', b'')
put('new', 'empty_both.bin', b'')
put('new', 'empty_added.bin', b'')
PY
}
build_dirs tree

emit() {
    local name=$1 kind=$2 fmt=$3 comp=$4 cks=$5 old=$6 new=$7; shift 7
    local diff="$OUT/data/$name.hdiff"
    local log
    if ! log=$("$HDIFFZ" -s-16 "$@" -c-"$comp" ${cks:+-C-$cks} -f "$old" "$new" "$diff" 2>&1); then
        return 1
    fi
    local windows
    windows=$(sed -n 's/.*windowCount: \([0-9]*\).*/\1/p' <<<"$log" | head -1)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$name" "$kind" "$fmt" "$comp" "${cks:-default}" "${windows:-0}" \
        "${old#$OUT/data/}" "${new#$OUT/data/}" >> "$MAN"
}

D="$OUT/data"
ok=0; skipped=0

for comp in no zstd zlib bz2 lzma lzma2; do
    for cks in "" no crc32 fadler64 xxh3 xxh128; do
        emit "w26_${comp}_${cks:-def}" file W26 "$comp" "$cks" "$D/old.bin" "$D/new.bin" \
            -w-64k-2k -WD-32k && ok=$((ok+1)) || skipped=$((skipped+1))
    done
    emit "sf20_$comp" file SF20 "$comp" "" "$D/old.bin" "$D/new.bin" -SD-64k && ok=$((ok+1)) || skipped=$((skipped+1))
    emit "h13_$comp"  file H13  "$comp" "" "$D/old.bin" "$D/new.bin"         && ok=$((ok+1)) || skipped=$((skipped+1))
done

for count in 16 31 32 33 63 64 65 96 129; do
    emit "wingeom_$count" file W26 zstd "" "$D/win${count}_old.bin" "$D/win${count}_new.bin" \
        -w-64k-2k -WD-32k && ok=$((ok+1)) || skipped=$((skipped+1))
done

for step in 4k 16k 256k 1m; do
    emit "winstep_$step" file W26 zstd "" "$D/old.bin" "$D/new.bin" -w-128k-4k -WD-$step && ok=$((ok+1)) || skipped=$((skipped+1))
done
for win in 32k-1k 128k-4k 512k-16k 2m; do
    emit "winsize_${win%%-*}" file W26 zstd "" "$D/old.bin" "$D/new.bin" -w-$win -WD-32k && ok=$((ok+1)) || skipped=$((skipped+1))
done

for fmt_opts in "W26:-w-64k-2k -WD-32k" "SF20:-SD-16k" "H13:"; do
    fmt=${fmt_opts%%:*}; opts=${fmt_opts#*:}
    for edge in empty one same shrink; do
        emit "edge_${edge}_${fmt}" file "$fmt" zstd "" "$D/${edge}_old.bin" "$D/${edge}_new.bin" \
            $opts && ok=$((ok+1)) || skipped=$((skipped+1))
    done
done

for comp in no zstd zlib bz2 lzma lzma2; do
    for cks in "" no crc32 fadler64 xxh3 xxh128; do
        tag="${comp}_${cks:-def}"
        emit "dir_w26_$tag"  dir W26  "$comp" "$cks" "$D/tree/old/" "$D/tree/new/" -w-64k-2k -WD-32k && ok=$((ok+1)) || skipped=$((skipped+1))
        emit "dir_sf20_$tag" dir SF20 "$comp" "$cks" "$D/tree/old/" "$D/tree/new/" -SD-64k         && ok=$((ok+1)) || skipped=$((skipped+1))
        emit "dir_h13_$tag"  dir H13  "$comp" "$cks" "$D/tree/old/" "$D/tree/new/"                 && ok=$((ok+1)) || skipped=$((skipped+1))
    done
done

if [ -f "$D/w26_no_no.hdiff" ]; then
    if python3 "$(dirname "$0")/make_extradata.py" "$D/w26_no_no.hdiff" "$D/w26_extradata.hdiff" 777 >/dev/null 2>&1; then
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            w26_extradata file W26 no no 0 old.bin new.bin >> "$MAN"
        ok=$((ok+1))
    else
        skipped=$((skipped+1))
    fi
fi

while IFS=$'\t' read -r name kind fmt comp cks windows old new; do
    case "$name" in
        w26_zstd_def|w26_no_def|sf20_zstd|h13_zstd|w26_zstd_fadler64|w26_zstd_crc32|w26_zstd_xxh3|dir_w26_zstd_def) ;;
        *) continue ;;
    esac
    src="$D/$name.hdiff"; dst="$D/${name}_corrupt.hdiff"
    python3 -c "
import sys
d=bytearray(open(sys.argv[1],'rb').read())
d[len(d)//2]^=0x40
open(sys.argv[2],'wb').write(bytes(d))
" "$src" "$dst" && printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "${name}_corrupt" corrupt "$fmt" "$comp" "$cks" "$windows" "$old" "$new" >> "$MAN"
done < "$MAN"

echo "generated=$ok skipped=$skipped manifest=$MAN"
echo "vectors: $(wc -l < "$MAN")"
