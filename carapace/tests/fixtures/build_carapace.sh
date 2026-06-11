#!/usr/bin/env bash
# Build a real layered ext4 carapace test fixture using only standard
# tools — `dmsetup` for the device-mapper plumbing, `losetup` for loop
# devices, `mkfs.ext4`/`mount`/`umount` for the filesystem, `sgdisk`
# for the GPT, `veritysetup` for hash trees. NO production-crate code
# is reused on the producer side.
#
# Per scute, the kernel itself populates the CoW (real persistent-store
# header + exception entries) by activating a writable dm-snapshot and
# letting it absorb a real ext4 mkfs / mount / write / unmount cycle.
# This makes the assembler reading the fixture a true differential test
# against (a) cryptsetup's veritysetup, (b) sgdisk's GPT writer, and
# (c) the kernel's own dm-snapshot persistent-store implementation.
#
# Usage:   build_carapace.sh <image_path> <n_scutes>
# Stdout:  trusted root hex (the top scute's root)
# Stderr:  progress lines, prefixed with "[fixture]"
#
# Requires: root (the script doesn't sudo internally).

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <image_path> <n_scutes>" >&2
    exit 64
fi
IMAGE="$1"
N="$2"

if ! [[ "$N" =~ ^[0-9]+$ ]] || [ "$N" -lt 1 ] || [ "$N" -gt 8 ]; then
    echo "n_scutes must be an integer in [1,8], got: $N" >&2
    exit 64
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "build_carapace.sh requires root (dmsetup/losetup/mount)" >&2
    exit 64
fi

# --- carapace partition type GUIDs (mirror src/partition.rs's pinned
#     vectors; the production code resolves by PARTUUID, not by these
#     type GUIDs, but sgdisk needs them for the fixture's GPT writer) ---
SCUTE_COW_RAW="11dd804ae1bf4ab398c1f9f48ceedbf1"
SCUTE_VERITY_RAW="40bb957129724547b5806a8cb13fd7d1"
BASE_FLAG_BIT=48

# --- per-layer geometry ---
# The production assembler sizes the top alias as
# data_blocks * 4096 bytes (= COW_BYTES). For ext4 mkfs at build time
# to produce a filesystem that fits in the assembled device, the
# writable snapshot's apparent size MUST equal COW_BYTES / 512.
COW_BYTES=$((16*1024*1024))                  # 16 MiB CoW per scute
LAYER_SECTORS=$((COW_BYTES / 512))           # snapshot/zero apparent size, matches COW
VERITY_BYTES=$((4*1024*1024))                # 4 MiB verity hash (ample for 16 MiB data)
GPT_ALIGN=2048                               # 1 MiB partition alignment

# --- partition byte sizes (post-veritysetup we know the verity blob is
# much smaller than 4 MiB; we pad each partition to the budgeted size) ---
COW_PART_BYTES="$COW_BYTES"
VERITY_PART_BYTES="$VERITY_BYTES"

WORK="$(mktemp -d -t carapace-fixture-XXXXXX)"
PREFIX="cf$$"   # dm name prefix (process-unique)
declare -a CLEANUP_MOUNTS=()
declare -a CLEANUP_DM=()        # dm device short names (we'll prefix at remove)
declare -a CLEANUP_LOOPS=()

cleanup() {
    set +e
    local m d l k
    # mounts first, in reverse
    for ((k=${#CLEANUP_MOUNTS[@]}-1; k>=0; k--)); do
        m="${CLEANUP_MOUNTS[$k]:-}"
        [ -n "$m" ] && { umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null; }
    done
    # then dm devices, in reverse (top-of-stack first)
    for ((k=${#CLEANUP_DM[@]}-1; k>=0; k--)); do
        d="${CLEANUP_DM[$k]:-}"
        [ -n "$d" ] && dmsetup remove -f "$PREFIX-$d" 2>/dev/null
    done
    # then loops
    for l in "${CLEANUP_LOOPS[@]:-}"; do
        [ -n "$l" ] && losetup -d "$l" 2>/dev/null
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

# Helper: standard mixed-endian raw → textual UUID for sgdisk.
raw_to_uuid() {
    python3 - "$1" <<'PY'
import sys
b = bytes.fromhex(sys.argv[1])
assert len(b) == 16
print("%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%02x%02x%02x%02x%02x%02x" % (
    b[3], b[2], b[1], b[0],
    b[5], b[4],
    b[7], b[6],
    b[8], b[9],
    b[10], b[11], b[12], b[13], b[14], b[15],
))
PY
}

COW_TYPE_UUID="$(raw_to_uuid "$SCUTE_COW_RAW")"
VERITY_TYPE_UUID="$(raw_to_uuid "$SCUTE_VERITY_RAW")"

attach_loop() {
    # echo loop path
    local file="$1"
    local lo
    lo="$(losetup --show -f "$file")"
    CLEANUP_LOOPS+=("$lo")
    echo "$lo"
}

dm_create() {
    # dm_create <short-name> <table-line>
    local name="$1"; shift
    echo "$@" | dmsetup create "$PREFIX-$name"
    CLEANUP_DM+=("$name")
}

dm_create_ro() {
    # dm_create_ro <short-name> <table-line> — for dm-verity, which
    # the kernel requires to be activated read-only.
    local name="$1"; shift
    echo "$@" | dmsetup create --readonly "$PREFIX-$name"
    CLEANUP_DM+=("$name")
}

# Build the read-only carapace stack for scutes [0..upto-1]. Leaves the
# top layer's mapper path in TOP_RO; if upto==0, TOP_RO=/dev/mapper/<zero>.
TOP_RO=""
build_ro_stack() {
    local upto="$1"
    local j
    # dm-zero
    dm_create "z" "0 $LAYER_SECTORS zero"
    TOP_RO="/dev/mapper/$PREFIX-z"
    for ((j=0; j<upto; j++)); do
        local cow_loop="${COW_LOOPS[$j]}"
        local verity_loop="${VERITY_LOOPS[$j]}"
        local salt="${SALTS[$j]}"
        local root="${ROOTS[$j]}"
        # dm-verity: 1 = version, sha256 = hash, data_block_size=4096,
        # hash_block_size=4096, num_data_blocks = COW_BYTES/4096,
        # hash_start_block = 1 (block 0 is superblock).
        local data_blocks=$((COW_BYTES / 4096))
        local table="0 $((data_blocks * 8)) verity 1 $cow_loop $verity_loop 4096 4096 $data_blocks 1 sha256 $root $salt"
        dm_create_ro "v$j" "$table"
        # dm-snapshot read-only-style: actually we use the same target;
        # the persistent header on the CoW prevents accidental writes
        # via the writable side.
        local origin="$TOP_RO"
        local cow="/dev/mapper/$PREFIX-v$j"
        dm_create "s$j" "0 $LAYER_SECTORS snapshot $origin $cow P 8"
        TOP_RO="/dev/mapper/$PREFIX-s$j"
    done
}

# Tear down whatever build_ro_stack put on top, plus a trailing writable
# snapshot if present. Walks CLEANUP_DM from the end until empty, but
# preserves CLEANUP_LOOPS (those persist across scutes).
teardown_dm() {
    local d k
    for ((k=${#CLEANUP_DM[@]}-1; k>=0; k--)); do
        d="${CLEANUP_DM[$k]}"
        dmsetup remove -f "$PREFIX-$d" >/dev/null 2>&1 || true
    done
    CLEANUP_DM=()
}

# --- 1. allocate cow + verity files (one pair per scute) ---
declare -a COW_FILES VERITY_FILES COW_LOOPS VERITY_LOOPS SALTS ROOTS
for ((i=0; i<N; i++)); do
    cf="$WORK/cow$i"
    vf="$WORK/verity$i"
    truncate -s "$COW_BYTES" "$cf"
    truncate -s "$VERITY_BYTES" "$vf"
    COW_FILES[i]="$cf"
    VERITY_FILES[i]="$vf"
done

echo "[fixture] building $N scutes (layer=$LAYER_SECTORS sectors, cow=$COW_BYTES B)" >&2

# --- 2. build each scute bottom-up ---
MNT="$WORK/mnt"
mkdir "$MNT"
for ((i=0; i<N; i++)); do
    echo "[fixture] scute $i: building..." >&2

    # 2a. attach this scute's CoW as a loop dev
    cow_loop="$(attach_loop "${COW_FILES[$i]}")"
    COW_LOOPS[i]="$cow_loop"

    # 2b. assemble the read-only stack of the chain so far [0..i-1]
    build_ro_stack "$i"

    # 2c. layer a writable snapshot on top whose CoW is this scute's cow
    dm_create "w" "0 $LAYER_SECTORS snapshot $TOP_RO $cow_loop P 8"
    snap_w="/dev/mapper/$PREFIX-w"

    # 2d. populate the layer
    if [ "$i" -eq 0 ]; then
        # Base: lay down a minimal ext4
        # Force 4096-byte block size: at read time the chain is exposed
        # via dm-verity at 4096-byte blocks, and ext4 refuses to mount
        # if its block size is smaller than the underlying device's.
        mkfs.ext4 -q -F -b 4096 -E nodiscard -L "carapace" \
            -U "00000000-0000-0000-0000-000000000001" "$snap_w"
    fi
    mount "$snap_w" "$MNT"
    CLEANUP_MOUNTS+=("$MNT")
    # Per-scute content
    echo "scute$i content" > "$MNT/scute$i.txt"
    if [ "$i" -gt 0 ]; then
        # Modify a file from a lower scute so we can verify cross-layer
        # composition later.
        echo "modified by scute$i" > "$MNT/scute0.txt"
    fi
    sync
    umount "$MNT"
    # pop the mount entry we just added
    unset 'CLEANUP_MOUNTS[${#CLEANUP_MOUNTS[@]}-1]'

    # 2e. tear down the dm stack — kernel finalizes CoW exceptions.
    # We must suspend the writable snap before remove to flush metadata.
    dmsetup suspend "$PREFIX-w" >/dev/null 2>&1 || true
    teardown_dm

    # The cow loop is no longer in use by dm; detach.
    losetup -d "$cow_loop"
    # Remove from CLEANUP_LOOPS to avoid double-detach on EXIT.
    CLEANUP_LOOPS=("${CLEANUP_LOOPS[@]/$cow_loop}")

    # 2f. compute salt for THIS scute and run veritysetup format on its
    # populated CoW. Salt = parent root for i>0; deterministic random
    # for base.
    if [ "$i" -eq 0 ]; then
        # Base scute: 32 zero bytes (the no-parent sentinel) + 32
        # deterministic-random suffix bytes (gives root_0 per-build
        # uniqueness; the deterministic-random choice is for test
        # reproducibility, not a spec requirement).
        salt="$(python3 -c '
import sys
zero = bytes(32)
suffix = bytes((j * 0x9b) & 0xff for j in range(32))
print((zero + suffix).hex())
')"
    else
        salt="${ROOTS[$((i-1))]}"
    fi
    SALTS[i]="$salt"

    out="$(veritysetup format \
        --hash=sha256 \
        --data-block-size=4096 \
        --hash-block-size=4096 \
        --format=1 \
        --salt="$salt" \
        "${COW_FILES[$i]}" "${VERITY_FILES[$i]}")"
    root="$(awk '/^Root hash:/ { print $3 }' <<<"$out")"
    if [ -z "$root" ]; then
        echo "[fixture] veritysetup format scute $i failed:" >&2
        echo "$out" >&2
        exit 1
    fi
    ROOTS[i]="$root"
    echo "[fixture] scute $i root=${root:0:16}..." >&2

    # 2g. Re-attach this scute's cow loop and create its dm-verity dev
    # for use by the NEXT scute's read-only stack rebuild.
    cow_loop="$(attach_loop "${COW_FILES[$i]}")"
    COW_LOOPS[i]="$cow_loop"
    verity_loop="$(attach_loop "${VERITY_FILES[$i]}")"
    VERITY_LOOPS[i]="$verity_loop"
done

# --- 3. compute total image size + per-scute partition layout ---
declare -a COW_START COW_END V_START V_END
total_sectors=$GPT_ALIGN
for ((i=0; i<N; i++)); do
    cs=$total_sectors
    ce=$(( cs + (COW_PART_BYTES/512) - 1 ))
    vs=$(( ((ce + 1 + GPT_ALIGN - 1) / GPT_ALIGN) * GPT_ALIGN ))
    ve=$(( vs + (VERITY_PART_BYTES/512) - 1 ))
    COW_START[i]=$cs;  COW_END[i]=$ce
    V_START[i]=$vs;    V_END[i]=$ve
    total_sectors=$(( ((ve + 1 + GPT_ALIGN - 1) / GPT_ALIGN) * GPT_ALIGN ))
done
total_sectors=$(( ((total_sectors + 64 + 33 + GPT_ALIGN - 1) / GPT_ALIGN) * GPT_ALIGN ))
total_bytes=$((total_sectors * 512))

# --- 4. allocate the GPT image and copy each scute's bytes in ---
truncate -s "$total_bytes" "$IMAGE"
for ((i=0; i<N; i++)); do
    dd if="${COW_FILES[$i]}" of="$IMAGE" bs=512 seek="${COW_START[$i]}" \
        count=$((COW_PART_BYTES/512)) conv=notrunc status=none
    dd if="${VERITY_FILES[$i]}" of="$IMAGE" bs=512 seek="${V_START[$i]}" \
        count=$((VERITY_PART_BYTES/512)) conv=notrunc status=none
done

# --- 5. write the GPT via sgdisk ---
sgdisk --zap-all "$IMAGE" >/dev/null 2>&1 || true
for ((i=0; i<N; i++)); do
    cs=${COW_START[$i]}; ce=${COW_END[$i]}
    vs=${V_START[$i]};   ve=${V_END[$i]}
    root="${ROOTS[$i]}"
    cow_partuuid="$(raw_to_uuid "${root:0:32}")"
    verity_partuuid="$(raw_to_uuid "${root:32:32}")"
    cow_slot=$((i * 2 + 1))
    verity_slot=$((i * 2 + 2))

    sgdisk \
        --new="$cow_slot:$cs:$ce" \
        --typecode="$cow_slot:$COW_TYPE_UUID" \
        --partition-guid="$cow_slot:$cow_partuuid" \
        --change-name="$cow_slot:cow$i" \
        "$IMAGE" >/dev/null

    sgdisk \
        --new="$verity_slot:$vs:$ve" \
        --typecode="$verity_slot:$VERITY_TYPE_UUID" \
        --partition-guid="$verity_slot:$verity_partuuid" \
        --change-name="$verity_slot:verity$i" \
        "$IMAGE" >/dev/null

    if [ "$i" -eq 0 ]; then
        sgdisk --attributes="$verity_slot:set:$BASE_FLAG_BIT" \
            "$IMAGE" >/dev/null
    fi
done

echo "${ROOTS[$((N-1))]}"
