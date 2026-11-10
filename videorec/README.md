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

### G-NET 블랙박스 (`.jdr` 파일)

카드에 `.jdr` 파일이 있으면 자동으로 G-NET 형식 처리가 켜집니다 (`--vendor jdr`로 강제, `--vendor none`으로 끔).
이 장치는 `.jdr` 파일을 미리 만들어 두고 링 방식으로 덮어쓰므로, 파일시스템상 "삭제된 파일"이 없습니다.
그래서 파일과 상관없이 볼륨 전체에서 청크를 찾아 채널별로, 녹화 시각 순서대로 이어 MP4를 만듭니다.
결과 파일 이름은 `jdr_<번호>_CH<채널>_<시작시각>.mp4`이고, `report.csv`의 `channel`, `start_time`, `end_time`에 기록됩니다.
그 파일의 현재 바퀴가 시작된 시각보다 이른 영상은 notes에 "earlier pass of the ring"으로 표시됩니다.
키프레임이 덮어써져 없는 조각도 버리지 않고, 같은 채널의 다른 키프레임에서 SPS/PPS를 빌려 `partial`로 저장합니다. 이런 조각은 화면이 깨져 보일 수 있습니다(`--jdr-drop-without-key`로 끔). `--stats-only`는 영상을 쓰지 않고 월별 프레임 수만 report.json의 `jdr_by_month`에 기록합니다.
청크 구조는 공급사 문서나 프로그램이 아니라 실제 카드의 녹화 데이터를 직접 보고 확인한 것이며, 다른 펌웨어 버전에서도 같은지는 확인하지 못했습니다. 오디오(AD)는 아직 디코딩하지 않습니다.

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
python3 tools/make_images.py /tmp/work   # 합성 SD카드 이미지 9종 생성 (ffmpeg, dosfstools, mtools, exfatprogs 필요)
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
