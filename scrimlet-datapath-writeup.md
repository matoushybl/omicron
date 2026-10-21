# Scrimlet datapath: softnpu codegen + sidecar (illumos driver) — measurements and plan

Status (eitri2 install in progress — NOT a final write-up; eitri R430 retired).

## 1. Purpose

The scrimlet datapath (eitri2, Dell R730, replacing retired eitri R430) is constrained
at layers. This document records what was measured, what remains, and what should
be benchmarked now that eitri is dead.

**Primary bottlenecks (per stack layer):**

1. **softnpu p4 codegen** — the `asic_program.so` built from the p4/sidecar_lite
   pipeline. Generated code was CPU-bound per packet (bitvec churn, allocator,
   selectors) before the 2026-10-14 fixes.
2. **sidecar-side illumos driver (dld)** — the raw-stream `dld_raw_str_hiwat`
   bump in the kernel (`/kernel/drv/amd64/dld`, GZ-wide) that the softnpu process
   shares. Second half of the datapath.

The MikroTik / house LAN is **not** a bottleneck. It was a ruled-out red herring:
a jumbo/MTU mismatch once caused a full blackhole (external services at MTU 8500
vs the bridge forwarding 1500), which was fixed by pinning the external path to
1500 — but 1500-MTU flow runs fine and is not the datapath limit. The external
MSS clamp (rack advertises MSS 1460 vs GZ's 8960, so sends are capped at 1460)
is the *external-path* ceiling and is deliberately out of scope here.

## 2. eitri2 baseline state (install in progress)

| Component | State (verified 2026-11-27) |
|---|---|
| vdevpool zpool | ONLINE, ashift=13, mounted /vdevpool, 449.50G free |
| m2_0.vdev / m2_1.vdev | 20 GiB each, root-owned, stock absolute paths (config.toml unchanged) |
| sidecar_softnpu zone | created, running |
| simnets | net0/net1/sc0_0/sc1_0/tfportrear0_0/tfportqsfp0_0 created |
| sc0_1 vnic | created, bridges fake_external_stub0 (etherstub, 9000) |
| softnpu | running in zone (stock npuzone binary from buildomat) |
| gateway ARP entry | 192.168.1.199 (fake_external0 02:08:20:c5:ce:b2) |
| proxy ARP | 192.168.1.100–192.168.1.120 → a8:e1:de:01:70:1d |
| sled-agent | **not installed yet** |
| SD boot chain | **not verified yet** |

## 3. Reference measurements (eitri R430, dead — dated, not eitri2)

### 3.1 softnpu p4 codegen — stock → fork → LTO → native (2026-10-13/14)

| Metric | Stock (8b672b07) | Fork fixes (24a21103) | LTO (12228a40) | Native codegen (a0fd8636) |
|---|---:|---:|---:|---:|
| Import wire avg | 4.68 MB/s | 11.21 MB/s | 11.00 MB/s | **18.88 MB/s (4.0×)** |
| Import wall | 548s | 247s | 253s | **142s** |
| Import drops | 65,766 | 46,652 | 53,021 | **11,460 (equilibrium 58/s)** |
| Guest 50MB pulls | 4.45 / 3.95 MB/s | 10.0 / 10.56 / 10.15 | 10.46 / 8.67 | **20.5 / 21.9 / 20.1** |
| Retrans/drop | 1.02 | 1.007 | ~1.0 | ~1.0 |
| Dtrace profile under load | bitvec 20.3%, compute 57% | same | same | **compute ≈ noise; syscall shell ~77%** |

Verdict: fork fixes (E0382 + eager-dump removal + table hot-path) ≈ real; LTO =
no measurable effect; native codegen = second real deliverable (1.72× wire over
LTO, ~2× pulls).

### 3.2 Runtime syscall surgery — softnpu.bin 4c64443f (2026-10-14)

| Metric | Old (f2241e3c) | New (4c64443f) | Δ |
|---|---:|---:|---:|
| import wall | 137–151s | **131s** | — |
| import wire | 17.9 MB/s | **20.6 MB/s** | +15% |
| guest 4×50MB pulls (avg) | 27.4 MB/s | **32.8 MB/s** | +20% |
| pollsys per packet | 1.18/pkt | **0.19/pkt** | −87% |
| putmsg | 1/pkt | 1/pkt | design floor |

Post-surgery profile: putmsg 32.9% (kernel TX, dominant), p4 run-closures 16.8%,
getmsg 12.9%, lwp_park ~10%, pollsys 4.6%.

### 3.3 illumos dld patch — `dld_raw_str_hiwat` bump

Pulse applies at stream-open (DLIOCRAW); tunable flips do NOT affect open streams —
requires softnpu relaunch (= reinstall on an installed rack).

| Date | Config | Impact |
|---|---|---:|
| 2026-10-13 | 256KiB pulse + NO_BUFMOD | guest pulls 4.45/3.95 MB/s (vs 100–500 KB/s historic, ~10–30×) |
| 2026-10-21 | 0x40000 in-code default, staged in BE | 19.4 / 22.4 MB/s, ~23/s drops |

Loss attribution: only loss ever observed = `rx_raw_canput_drop` on the sc0_1 raw
stream. kstat: `dld:0:dld_drops` (`rx_raw_canput_drop / rx_fastpath_canput_drop /
rx_unitdata_canput_drop / tx_discard`) — external leg only; fastpath/unitdata/tx
always 0.

### 3.4 Combined baseline → optimized (eitri, dead — eitri2 not yet measured)

| Path | Before | After (native codegen + surgery + dld) |
|---|---:|---:|
| Import wire | 4.68 MB/s | ~20.6 MB/s |
| Guest pulls | 4.45 / 3.95 MB/s | ~32.8 MB/s (real external network path) |
| Historic unpatched baseline | 100–500 KB/s | ~40–200× |

## 4. What is still unmeasured / incomplete

1. **dld hiwat sweep** — only 256K and 0x40000 points; whole range unmeasured
   (never run — restart-binding).
2. **Bufmod flip** — binary supports both; never measured.
3. **TxRing idle-hysteresis** — queued (~0.87/pkt residual park churn); never
   implemented.
4. **eitri2 benchmarks** — nothing measured yet. Must use the R730 (baseline =
   stock asic + stock softnpu.bin + no dld patch):
   - converge rack to 19 zones + nexus 302
   - sparse 3.5GB import: wire MB/s, wall, drops (`dld_drops` kstat, equilibrium
     58/s class)
   - 4×50MB guest pulls (high-RTT path)
   - **then** the optimized recipe (native codegen a0fd8636 + syscall surgery
     4c64443f + dld 0x40000 pulse) and re-measure.
   Expected: ~20 MB/s import / ~32 MB/s pulls, based on the eitri reference.
5. **The MSS/jumbo clamp** (rack answers MSS 1460 vs GZ's 8960) — parked
   2026-10-14, reopen trigger: eitri2 real-network pads plateau at ~20 MB/s
   again. Note this caps external-path but is irrelevant for the system-side
   benchmarks.
6. **~3% loss during active periods** (suspects host vnic/etherstub leg or
   xde/OPTE) — never closed out.

## 5. Hard laws (must respect before re-benchmarks)

- Never restart sled-agent (destroys control-plane zones; requires full reinstall).
- Don't touch softnpu between create and convergence.
- Local-storage disks wedge uninstall on full-size zvols
  (`pfexec zfs destroy -r oxp_<sled>/local_storage_unencrypted` fixes).
- `oxide disk import` is content-based zero-skip: sparse pads collapse to 0
  bytes at zero-regions — a "slow" CLI is often a 0-skip artifact, not the
  datapath. Verified via a zero-free random file in the upload-debug memory.
- `dladm -s` columns: $5 = OPACKETS, not bytes.
- Treat the reference numbers as eitri-R430-dated; eitri2 numbers must come
  from eitri2 racks, not be transcribed.

## 6. Reference provenance

All eitri-reference numbers: ~/omicron memory dir (softnpu-native-codegen-ab-results.md,
softnpu-syscall-surgery-ab-results.md, dld-raw-str-hiwat-pulse-patch.md,
external-mtu-mss-blackhole.md, softnpu-mss-ceiling.md). All dated 2026-10-13/14/21;
eitri R430, retired.
