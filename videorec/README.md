# videorec — 블랙박스·CCTV 삭제 영상 복구 (MVP)

블랙박스 SD카드와 CCTV 디스크 이미지에서 삭제되거나 포맷된 영상을 복구하는 명령줄 도구입니다.
공개 표준(ITU-T H.264/H.265, ISO/IEC 14496-12/15, Microsoft FAT32/exFAT 규격)만으로 독자 구현했습니다.
설계 배경과 출처는 [`../docs/video-recovery/DESIGN.md`](../docs/video-recovery/DESIGN.md)를 참고하세요.

## 복구 단계

1. **파일시스템 기록 (FAT32 / exFAT)**: 삭제된 디렉터리 엔트리에서 이름, 시각, 시작 위치를 찾습니다. MP4/AVI 구조를 끝까지 확인한 경우에만 원본 그대로 저장합니다.
2. **컨테이너 카빙**: 포맷 등으로 파일시스템 기록이 사라진 경우, 섹터마다 `ftyp`(MP4/MOV) 또는 `RIFF AVI`를 찾습니다. 구조가 온전하다고 확인된 파일만 원본 그대로 저장합니다.
3. **프레임 카빙 (핵심)**: 남은 영역 전체에서 H.264/H.265 프레임을 직접 찾아 스트림으로 다시 묶고, 프레임 바이트는 바꾸지 않은 채 MP4로 감쌉니다. 파일시스템이 없거나, 헤더가 덮어써졌거나, 조각난 경우에도 동작합니다.
   - MP4 내부 형식(길이 접두)과 Annex B 형식(시작 코드: AVI, 원시 스트림)을 모두 인식합니다.
   - 디코딩에 필요한 SPS/PPS는 다음 순서로 찾습니다: ① 스트림 안 → ② 디스크 어딘가에 남은 `avcC`/`hvcC` → ③ `--reference`로 지정한 정상 파일
   - 오탐을 줄이기 위해 슬라이스 헤더의 각 값이 허용 범위 안인지, frame_num/POC가 이어지는지, 프레임 크기가 원본 화면 크기를 넘지 않는지 확인합니다.

원본 이미지는 읽기 전용으로만 엽니다.


## GUI (화면으로 쓰기)

`videorec.exe`를 더블클릭하거나 `videorec gui`를 실행하면 브라우저에 화면이 열립니다. 화면은 이 PC 안(127.0.0.1)에서만 열리고, 시작할 때 받은 주소 안의 열쇠값이 있어야 조작할 수 있습니다. 창을 닫으면 끝납니다.

1. **원본 선택:** 이미지 파일을 고르면 파일시스템, .jdr 여부, CCTV 녹화기 디스크 서명(Hikvision, Dahua, Honeywell)을 보여 줍니다.
2. **스캔:** 범위, 찾을 대상, 시각·카메라, 형식(AVI/MP4), 폴더 정리, 세션 저장을 고르고 시작합니다. 진행률과 남은 시간이 나옵니다. 화면은 명령줄의 `recover`를 그대로 실행합니다.
3. **결과:** 카메라별 하루 타임라인과 목록을 보고 거릅니다. 고른 영상은 기본 플레이어로 재생하거나 폴더에서 열 수 있습니다. 원본 위치와 SHA-256도 함께 보입니다.
4. **내보내기:** 선택한 영상, 보이는 목록, 전부 중 하나를 다른 폴더에 복사하고 해당 보고서를 함께 씁니다. 저장한 세션에서 다시 만들 수도 있습니다(`export`).

**물리 디스크 직접 읽기 (Windows):** 원본 선택의 맨 위 목록에 `💽 디스크 N`(디스크 관리의 번호와 같음)이 나옵니다. 명령줄에서는 `videorec recover \\.\PhysicalDrive3 -o D:\out`처럼 씁니다. 관리자 권한으로 실행해야 열립니다. 읽기 전용으로 열고, 섹터 단위로 정렬해 읽으며, 읽을 수 없는 섹터는 0으로 채운 뒤 `summary.unreadable_sectors`에 개수를 남깁니다. 저장 위치가 같은 디스크의 볼륨이면 스캔을 거부합니다. Windows가 디스크를 인식하지 못하면 "포맷하시겠습니까?" 창을 띄울 수 있는데, 절대 누르지 말고 취소하세요. 실제 디스크에서는 아직 검증 전입니다.

**뒤가 잘린 MP4:** 색인(moov)이 온전한데 데이터가 중간부터 잘리거나 덮어쓰인 MP4는 남은 앞부분을 모든 트랙(소리 포함)과 원래 시간 정보 그대로 `carved_…_part.mp4`로 저장합니다(`src/mp4fix.rs`, ISO/IEC 14496-12의 샘플 표를 남은 샘플까지만 줄임). 영상 샘플은 NAL 길이 사슬이 표와 정확히 맞는지로 확인하고, 손상 바로 앞의 한 프레임은 본문을 확인할 수 없어 뺍니다. 색인이 사라진 파일은 전처럼 프레임 찾기로 넘어갑니다(영상만).

**MPEG-1/2:** 영상 스트림(ISO/IEC 11172-2, 13818-2)과 프로그램 스트림(13818-1)을 찾아 `.mpg`로 저장합니다(`src/mpeg12.rs`). 끄려면 `--no-mpeg12`.

**XFS, Btrfs:** 살아 있는 파일과 빈 공간을 읽고, 삭제된 파일을 이름·크기·시각과 함께 찾습니다. XFS는 삭제 때 inode의 블록 목록이 지워지므로 로그에 남은 옛 inode 사본을 씁니다(`src/xfs.rs`, 출처: XFS Algorithms & Data Structures, xfs_format.h). Btrfs는 쓰기마다 트리 블록을 새로 만들므로 남아 있는 옛 블록을 체크섬 확인 후 씁니다(`src/btrfs.rs`, 출처: 커널 Btrfs 문서 On-disk format, btrfs_tree.h). 둘 다 데이터 블록이 다른 데 다시 쓰이지 않은 경우만 복구합니다.

**HFS+, APFS, JFS, ReiserFS:** 살아 있는 파일과 빈 공간을 읽고 삭제된 파일을 찾습니다. JFS는 삭제 후에도 inode에 크기와 블록 목록이 남습니다(`src/jfs.rs`, 출처: 리눅스 커널 fs/jfs 헤더). ReiserFS는 저널과 빈 블록에 남은 옛 리프 노드를 씁니다(`src/reiser.rs`, 출처: 커널 fs/reiserfs/reiserfs.h). APFS는 빈 블록에 남은 옛 트리 노드를 체크섬 확인 후 씁니다(`src/apfs.rs`, 출처: Apple File System Reference). HFS+는 카탈로그 노드의 빈 자리와 저널에 남은 레코드를 씁니다(`src/hfsplus.rs`, 출처: Apple TN1150). ReiserFS와 APFS의 빈 블록 탐색은 64GiB 이하 볼륨에서만 하며 `VIDEOREC_FS_SCAN_GB`로 바꿉니다. 시험 이미지는 `tools/vm_fs_images.sh`(QEMU와 우분투 커널 모듈)로 만들었고, 맥에서 실제로 지운 이미지로는 검증하지 못했습니다.

**Motion JPEG 2000:** JPEG 2000 코드스트림(ISO/IEC 15444-1)을 찾아 AVI로 저장합니다(`src/mj2k.rs`). 끄려면 `--no-mj2k`.

**볼륨 고르기:** `--volume 2`(또는 `1,3`)로 `info`가 보여 주는 순서의 볼륨만 검사합니다.

**큰 디스크 검사 시간:** 찾을 대상(MP4/AVI 조각, H.264/H.265, MJPEG, MPEG-4)을 원본 한 번 읽기로 함께 찾습니다(`src/multi.rs`; 전에는 대상마다 한 번씩, 네 번 읽었습니다). 최근에 읽은 블록 512 MiB를 메모리에 두고 검사기들이 같이 씁니다(`VIDEOREC_CACHE_MB`로 조절). 결과는 대상별로 따로 읽던 때와 같도록 순서를 맞췄고, 시험 이미지 15종에서 항목·해시가 같음을 확인했습니다. 예외: 프레임 찾기가 이미 진행된 뒤에 디스크에서 새 설정 정보(avcC/hvcC)가 나오면 프레임 찾기를 처음부터 한 번 더 합니다(모두 두 번 읽기, report.json의 `read_passes`). Dahua DVR 프레임 검사와 .jdr 검사는 그 앞에 따로 한 번 읽습니다. 필요 없는 대상은 `--no-containers`, `--no-h26x`, `--no-mjpeg`, `--no-mpeg4`로 끄고, `--range-gb 0-50`처럼 앞부분만 먼저 검사해 볼 수 있습니다. 화면에서는 "H.264/H.265 프레임만 찾기" 버튼과 검사 구간 칸으로 같은 일을 합니다.

CCTV 녹화기 디스크는 서명으로 알아보기만 합니다. 전용 파일시스템은 아직 읽지 못하고, 영상은 프레임 카빙으로 찾습니다. 근거는 docs/video-recovery/CCTV_FS_SOURCES.md에 있습니다.

## 사용법

```bash
cargo build --release
# 이미지 정보 (파티션, 파일시스템, 삭제된 엔트리)
./target/release/videorec info card.img
# 복구
./target/release/videorec recover card.img -o out/
# 같은 기기에서 찍은 정상 파일이 있으면 함께 지정 (SPS/PPS, 프레임 속도)
./target/release/videorec recover card.img -o out/ --reference good.mp4
# 파일을 미리 할당해 두고 그 안에 덮어쓰는 장치: 사용 중인 영역까지 검사
./target/release/videorec recover card.img -o out/ --scope all
```

### 거르기, 폴더 정리, AVI 출력

```bash
# 2026-11-01 08시~10시, 카메라 1과 3만, AVI로, 카메라/날짜 폴더에
videorec recover card.img -o out/ --from "2026-11-01 08:00" --to "2026-11-01 10:00" --camera 1,3 --format avi --group camera-date
```

`--from/--to/--camera`는 녹화기 시각과 채널이 기록된 형식(현재 G-NET `.jdr`)에만 적용됩니다. 카메라 번호는 녹화기 채널 + 1입니다(CH00 = CAM1).
AVI는 OpenDML 형식이라 1GB가 넘는 클립도 저장됩니다. AVI는 고정 프레임 속도라, 녹화 시각 간격은 MP4에서만 그대로 유지됩니다.
Motion-JPEG 프레임이 이어져 있으면 MJPEG AVI로 복구합니다(`carved_mjpeg`). 프레임 속도를 모르면 10fps로 가정합니다(`--fps`로 지정).

### 스캔 세션 저장 후 나중에 내보내기

```bash
videorec recover card.img -o out/ --save-session D:/sessions/card.json   # 세션 파일은 원본과 다른 디스크에
videorec export D:/sessions/card.json -o out2/ --from 2026-11-01 --camera 1 --format avi
```

세션 파일에는 찾은 데이터의 위치만 들어 있고 영상은 없습니다. `export`는 원본 전체를 다시 읽지 않고 청크 헤더만 따라가므로 빠릅니다. 원본 크기와 앞뒤 1MiB의 해시가 다르면 거부합니다. 현재는 G-NET `.jdr` 결과만 저장합니다.

### G-NET 블랙박스 (`.jdr` 파일)

카드에 `.jdr` 파일이 있으면 자동으로 G-NET 형식 처리가 켜집니다 (`--vendor jdr`로 강제, `--vendor none`으로 끔).
이 장치는 `.jdr` 파일을 미리 만들어 두고 링 방식으로 덮어쓰므로, 파일시스템상 "삭제된 파일"이 없습니다.
그래서 파일과 상관없이 볼륨 전체에서 청크를 찾아 채널별로, 녹화 시각 순서대로 이어 MP4를 만듭니다.
결과 파일 이름은 `jdr_<번호>_CH<채널>_<시작시각>.mp4`이고, `report.csv`의 `channel`, `start_time`, `end_time`에 기록됩니다.
그 파일의 현재 바퀴가 시작된 시각보다 이른 영상은 notes에 "earlier pass of the ring"으로 표시됩니다.
키프레임이 덮어써져 없는 조각도 버리지 않고, 같은 채널의 다른 키프레임에서 SPS/PPS를 빌려 `partial`로 저장합니다. 이런 조각은 화면이 깨져 보일 수 있습니다(`--jdr-drop-without-key`로 끔). `--stats-only`는 영상을 쓰지 않고 월별 프레임 수만 report.json의 `jdr_by_month`에 기록합니다.
청크 구조는 공급사 문서나 프로그램이 아니라 실제 카드의 녹화 데이터를 직접 보고 확인한 것이며, 다른 펌웨어 버전에서도 같은지는 확인하지 못했습니다. 소리(AD 청크)는 6456.raw에서 직접 확인한 결과 모노 16비트 PCM, 8000Hz입니다. 청크 하나에 1024바이트(512샘플)가 들어 있고 약 64ms마다 기록됩니다. 태그 숫자(00AD/01AD/02AD)로 마이크가 나뉘지는 않습니다. 세 태그를 시간순으로 이어야 하나의 연속된 소리가 됩니다. 그래서 복구한 클립마다 같은 시간대의 소리를 함께 넣습니다(MP4는 `sowt` 트랙, AVI는 WAVE PCM 스트림). 소리를 바꾸거나 다시 인코딩하지는 않습니다. 200ms보다 긴 빈 구간은 무음으로 채워 영상과 시간을 맞춥니다. 소리를 빼려면 `--no-audio`를 쓰세요. 이 형식은 G-NET CATCHON(FW 2.0.7.8) 한 대에서만 확인했습니다.

결과물은 `out/`에 저장되고, `report.json`과 `report.csv`에 항목별 원본 위치(오프셋), SHA-256, 프레임 수, 상태가 기록됩니다.

| 상태 | 의미 |
|---|---|
| `ok` | 구조 확인을 통과했거나, 끊김 없이 복구된 스트림 |
| `partial` | 첫 키프레임 이전 프레임이 빠졌거나, 검증 없이 조각을 이어 붙인 파일 |
| `damaged` | 파일시스템 기록은 있지만 데이터가 덮어써짐 (남은 부분은 프레임 카빙으로 처리) |
| `needs_parameter_sets` | 프레임은 찾았지만 SPS/PPS가 없음 → `--reference` 필요 |

Windows용 빌드: `cargo build --release --target x86_64-pc-windows-gnu` (mingw-w64 필요)

## 테스트

```bash
python3 tools/make_images.py /tmp/work   # 합성 이미지 10종 생성 (ffmpeg, dosfstools, mtools, exfatprogs 필요)
python3 tools/evaluate.py /tmp/work      # 복구 후 원본과 프레임 단위로 비교
cargo test
```

`evaluate.py`는 원본 영상을 디코딩한 각 프레임의 해시(ffmpeg framemd5)가 복구 결과에 몇 개나 들어 있는지 셉니다.

### 현재 결과 (합성 이미지, 2026-11-09)

| 시나리오 | 내용 | 결과 |
|---|---|---|
| fat32_deleted | FAT32에서 MP4 2개, AVI 1개 삭제 | 3개 모두 원본과 동일 |
| fat32_formatted | 녹화 후 빠른 포맷 (MOOV 없는 녹화 중단 파일 포함) | 4개 원본과 동일, 중단 파일 300/300 프레임 |
| fat32_fragmented | 조각난 파일 + 앞부분 300KB가 덮어써진 파일 | 240/240, 120/180 프레임 (덮어쓴 구간 이후 첫 키프레임부터) |
| exfat_deleted | exFAT에서 3개 삭제 (그중 1개는 MOOV 없음) | 2개 원본과 동일, 1개 300/300 프레임 |
| raw_noFS | 파일시스템 없음, 파일 헤더 손실 (H.264 + H.265) | 210/240, 150/180 프레임 |
| avi_overwritten | AVI 헤더와 앞부분 덮어써짐 | 119/150 프레임 |
| back_to_back | 파일 3개가 빈틈없이 이어서 기록됨 | 180/180, 180/180, 240/240 프레임 |
| ntfs_gpt | GPT 디스크의 NTFS에서 MP4 3개 삭제 (H.265, MOOV 없는 파일 포함) | 2개 원본과 동일, MOOV 없는 파일 300/300 프레임 |
| ext4_deleted | ext4에서 MP4 3개를 리눅스 커널로 삭제 (inode의 extent가 지워진 상태) | 저널의 inode 사본으로 2개 원본과 동일, MOOV 없는 파일 300/300 프레임, 이름 복원 |
| mjpeg_raw | AVI 헤더 없는 MJPEG 프레임 + 사진 1장 | 59/60 프레임 (잘린 첫 프레임 제외), 사진은 영상으로 만들지 않음 |
| jdr_ring | G-NET식 3채널 링 파일 4개, 2바퀴째가 GOP 중간까지 덮어씀 | 2바퀴째 3채널 375/375, 1바퀴째 남은 부분 각 210/600 원본 동일 + 키프레임 앞 15프레임은 손상 상태로 저장 |

- 무작위 데이터 3GB(+SPS/PPS 후보 존재)에서 가짜 스트림 0개, 스캔 속도 약 400MB/s (이 컨테이너 기준)
- 프레임이 빠진 시나리오는 모두 손실 구간 이후 첫 키프레임부터 복구됩니다. 키프레임 없이 그 이전 프레임은 디코딩할 수 없습니다.

## 한계 (아직 안 되는 것)

- **실제 장치 이미지로는 아직 시험하지 않았습니다.** 위 결과는 모두 ffmpeg로 만든 합성 데이터입니다.
- B-프레임이 있는 스트림은 재생 순서 정보(ctts)를 만들지 않아 순서가 틀릴 수 있습니다 (미검증).
- 프레임 카빙 결과에는 오디오가 없습니다.
- MP4 길이 접두는 4바이트로 가정합니다.
- MJPEG, Hikvision/Dahua 등 CCTV 전용 포맷은 아직 지원하지 않습니다 (설계 문서의 다음 단계).
- H.265의 이어 붙이기 검증은 H.264보다 약합니다 (POC 연속성까지만 확인).
- Windows 실행 파일은 빌드만 확인했고, 실제 Windows에서 실행해 보지는 않았습니다.
