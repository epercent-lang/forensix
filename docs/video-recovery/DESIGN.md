# CCTV·블랙박스 삭제 영상 복구 프로그램 — 설계 문서 (초안 v0.1)

작성일: 2026-11-09 · 상태: 검토 대기

> **표기 규칙**
> - **[확인]** 이번 작업에서 원문(페이지·파일)을 직접 확인한 사실. 출처를 붙임.
> - **[표준]** 공개 표준 문서에 있는 내용. 문서 이름을 붙였지만 이번 작업에서 원문 페이지를 다시 열어 보지는 않았음. 구현 전에 원문으로 재확인해야 함.
> - **[미확인]** 추정이거나 아직 근거를 찾지 못한 내용. 구현 전에 반드시 검증해야 함.

---

## 0. 개발 원칙: 역설계가 아니라 "독자 구현"

업로드된 두 프로그램은 상용 제품입니다. 이 제품들의 코드나 UI를 디컴파일·복제하지 않고, 공개된 표준과 논문만으로 독자적으로 구현합니다.

- **[확인]** myCCTV Recovery EULA 7.1항은 디컴파일, 디스어셈블, 리버스 엔지니어링, 2차적 저작물 작성을 금지합니다. 또 역설계로 알게 된 내용을 권리자에게 통지하고 기밀로 다루도록 요구합니다. 준거법은 우크라이나법입니다(17항). — [myCCTV Recovery EULA (PDF)](https://www.sysdevlabs.com/licenses/mycctv_recovery_eula.pdf)
- 따라서 두 exe는 다음 두 가지 용도로만 씁니다.
  1. **지원 범위 파악**: 파일 메타데이터와, 실행 파일에 평문으로 들어 있는 포맷 이름 문자열만 봅니다(아래 1.2절). 코드 흐름은 분석하지 않습니다.
  2. **블랙박스 벤치마크**: 같은 테스트 디스크 이미지를 두 제품과 우리 프로그램에 각각 돌려 결과(복구된 프레임 수, 재생 가능 여부)만 비교합니다.
- 프로젝트 메모리에 있는 "앱 복제/역설계 스킬" 저장소 목록은 이 원칙에 맞지 않으므로 이번 작업에는 쓰지 않았습니다.

> 법률 자문이 아닙니다. 상용화 전에는 변호사 검토를 권합니다.

---

## 1. 참고 제품 조사 (비침습 조사만 수행)

### 1.1 파일 메타데이터 **[확인: 업로드된 exe의 버전 리소스]**

| 업로드 파일 | 제품명 | 버전 | 제조사 | SHA-256 |
|---|---|---|---|---|
| cctvrec-kr.exe | CCTV Recovery (원본 파일명 cctvrec_kr.exe) | 3.27.0.7734 | LLC SysDev Laboratories | 1cc27ea8…bbcfa |
| blackboxrec-kr.exe | BlackBox Recovery (원본 파일명 bboxrec_kr.exe) | 1.22.0.7981 | LLC SysDev Laboratories | b0dfe006…7bea3 |

- 두 파일 모두 Windows x64 GUI 실행 파일(PE32+)입니다.
- 두 파일 모두 버전 리소스상 제조사가 SysDev Laboratories입니다. **[미확인]** "복구천사"가 SysDev 제품의 국내 판매처(한국어판)인지는 확인하지 못했습니다. 웹 검색에서 둘의 관계를 밝히는 자료를 찾지 못했습니다.

### 1.2 실행 파일 안의 포맷 이름 문자열 **[확인: `strings` 출력]**

이 목록은 지원 포맷을 추정하는 단서로만 씁니다. 문자열이 있다고 해서 실제로 그 기능을 지원한다는 보장은 없습니다.

- **파일시스템**: FAT32, exFAT, NTFS, HFS+, SGI XFS, UFS 계열 (두 제품 공통)
- **CCTV 전용 포맷 이름 (두 제품 공통)**: `HIKvision CCTV`, `DHFS CCTV`, `WFS CCTV`, `WGFS CCTV`, `MDFS CCTV`, `TPFS CCTV`, `Rsfs CCTV`, `SFMR CCTV`, `NF3 CCTV`, `Aura CCTV`, `dVfE CCTV`, `JDR Video`
- **CCTV Recovery에만 있음**: `DHAV CCTV`, `SAMSUNG recording`, `Microdigital recording`, `Commax Video files`, `QVR Pro Video`, `Smart NVR`, `Tibet DFS Video`, `HDdvr1000`, `XyView DAT`, `FDVR DAT`, `FTVT DAT`, `EH60`, `SBD CCTV`, `XQ CCTV` 등
- **BlackBox Recovery에만 있음**: `NxFS AVI`, 출력 파일명 패턴 `_CH%d.avi`, `nodate_CH%d_%s@%08llX.avi`. 채널별로 나눈 AVI를 출력하는 것으로 보입니다 **[미확인]**.
- **컨테이너·코덱 (공통)**: AVI, MP4, MOV(`MOV moov (32bit)`, `MOV mdat (64bit)`), 3GP, MKV, WMV, MPEG-TS, H264, H265
- 두 바이너리에 H.265 비트스트림 필드 이름(`active_video_parameter_set_id`, `colour_remap_video_signal_info_present_flag` 등)이 있습니다. 비트스트림을 직접 파싱해 프레임을 검증하는 것으로 보입니다 **[미확인]**.

### 1.3 공식 기능 목록 **[확인]** — [myCCTV Recovery 매뉴얼: Features overview](https://www.sysdevlabs.com/manuals/mycctv/software-overview-and-main-features.php)

- 일반 FS: NTFS, FAT/FAT32, exFAT, HFS+, APFS, Ext2/3/4, XFS, JFS, ReiserFS, Btrfs
- "CCTV file systems: WFS, DHFS, HIK, Mirage, Pinetron, BKFL, etc."
- 코덱: Motion-JPEG, MJPEG2000, MPEG-4 Part 2, H.264, H.265, MPEG-1/2. 오디오: PCM, ADPCM, G.711
- 스트림 포맷 예: dhav(DHFS), dch264(DHFS), idis, idis2, everfocus, commax, hav, rsfm 등 수십 종
- 기능: 디스크 이미지 지원, 프레임 유효성 검증, 프레임 내용은 그대로 두고 AVI로 재포장, 날짜·시간·카메라별 필터, 미리보기, 스캔 세션 저장

**시사점**: 이 제품의 강점은 파일시스템 메타데이터에 의존하지 않는 것으로 보입니다. 원시 프레임을 직접 찾아 검증한 뒤 시간순·채널별로 다시 묶는 방식입니다. 이 해석은 매뉴얼의 "frames are checked for validity and placed in AVI" 설명에 근거한 것입니다. 우리 설계도 이 방식을 중심에 둡니다.

---

## 2. 지원 대상과 우선순위

| 우선순위 | 대상 | 저장 구조 | 공개 자료 수준 |
|---|---|---|---|
| P1 | 블랙박스 SD카드 (일반형) | FAT32 / exFAT + MP4·AVI | 높음 (MS 공개 규격 + ISO 표준) |
| P2 | Hikvision DVR/NVR | 독자 FS("HIK") | 중간 (논문 2편 이상) |
| P3 | Dahua DVR/NVR | DHFS 4.x, DHAV 프레임 | 중간 (논문 1편) |
| P4 | 기타 (Honeywell, IDIS, Samsung/Hanwha 등) | 독자 포맷 | 낮음~중간 (Honeywell은 논문 있음) |
| 범용 | 모든 장치 | 원시 H.264/H.265 카빙 | 높음 (ITU-T 표준) |

**[미확인]** 국내 블랙박스(아이나비, 파인뷰, 팅크웨어 등) 가운데 일부는 FAT 대신 전용 파일시스템을 쓴다는 말이 있지만, 근거 자료를 아직 찾지 못했습니다. 실제 SD카드 이미지로 확인이 필요합니다.

---

## 3. 대상별 복구 기법

### 3.1 범용: H.264 / H.265 NAL 카빙 (모든 장치의 최후 수단)

- **[표준]** ITU-T H.264 Annex B: NAL 유닛은 시작 코드 `00 00 01` 또는 `00 00 00 01`로 구분됩니다. NAL 헤더 첫 바이트의 하위 5비트가 nal_unit_type입니다(7=SPS, 8=PPS, 5=IDR, 1=non-IDR).
- **[표준]** ITU-T H.265: NAL 헤더는 2바이트이고, nal_unit_type은 첫 바이트의 비트 1~6입니다(32=VPS, 33=SPS, 34=PPS, 19/20=IDR).
- 처리 절차:
  1. 디스크 전체를 섹터 단위로 스캔해 시작 코드와 SPS/PPS 후보를 찾습니다.
  2. SPS를 실제로 파싱해(해상도, profile 값이 정상 범위인지) 오탐을 제거합니다.
  3. 슬라이스 헤더의 frame_num / POC 연속성으로 같은 스트림을 묶습니다.
  4. 결과를 Annex B 원시 스트림으로 저장한 뒤 MP4/AVI로 재포장합니다(3.6절).
- 한계: 타임스탬프와 채널 정보는 대개 컨테이너나 장치 고유 헤더에 있습니다. 그래서 원시 NAL만으로는 "언제, 몇 번 카메라" 영상인지 알기 어렵습니다.

### 3.2 블랙박스 SD카드: FAT32 / exFAT

- **[표준]** Microsoft FAT 규격: 삭제된 파일은 디렉터리 엔트리의 첫 바이트가 `0xE5`로 바뀌고 FAT 체인이 0으로 해제됩니다. 시작 클러스터와 크기는 엔트리에 남는 경우가 많습니다.
- **[표준]** Microsoft exFAT 규격(공개 문서): 삭제하면 디렉터리 엔트리 타입의 InUse 비트가 0으로 바뀝니다. 엔트리에 NoFatChain 플래그가 있으면 연속 할당이므로 시작 클러스터와 길이만으로 복원할 수 있습니다.
- 처리 절차:
  1. **메타데이터 복구**: 삭제된 엔트리에서 이름, 시작 클러스터, 크기, 시각을 얻어 연속 할당을 가정하고 추출합니다.
  2. **포맷 후 복구**: 디렉터리가 사라졌으면 3.3절의 MP4/AVI 카빙으로 넘어갑니다.
  3. **조각난 파일**: 블랙박스는 여러 채널을 번갈아 쓰기 때문에 파일이 섞여 저장될 수 있습니다 **[미확인: 장치마다 다름]**. 이 경우 3.1절의 프레임 단위 재조립이 필요합니다.
- 참고 구현 **[확인]**: [ccbrown/teslacam-recovery](https://github.com/ccbrown/teslacam-recovery/blob/master/README.md)는 FAT32/exFAT 전체를 클러스터 단위로 스캔해 삭제된 MP4를 카빙합니다. 작성자는 Tesla가 파일을 대체로 연속으로 기록한다고 적었습니다.

### 3.3 MP4 / MOV / 3GP 카빙과 복구

- **[표준]** ISO/IEC 14496-12 (ISO BMFF): 박스는 `[4바이트 크기][4바이트 타입]` 구조입니다(`ftyp`, `moov`, `mdat`, `free`). 크기가 1이면 64비트 크기가 뒤따릅니다.
- **[표준]** ISO/IEC 14496-15: MP4 안의 H.264/H.265는 시작 코드 대신 길이 접두(보통 4바이트) 형식으로 저장되고, SPS/PPS는 `moov` 안의 `avcC`/`hvcC`에 있습니다.
- 경우별 처리:
  - `ftyp`부터 `moov`까지 온전함 → 박스 크기를 따라 그대로 추출합니다.
  - 녹화 중 전원 차단 등으로 `moov`가 없음 → `mdat` 안의 길이 접두 NAL을 순서대로 읽어 샘플 테이블(`stsz`, `stco`, `stts`, `stss`)을 새로 만듭니다. 이때 같은 기기에서 찍은 정상 파일을 "참조 파일"로 써서 SPS/PPS와 코덱 설정을 가져옵니다.
  - 참고 구현 **[확인]**: [anthwlock/untrunc](https://github.com/anthwlock/untrunc) (GPL-2.0)는 같은 카메라의 정상 파일을 참조해 잘린 mp4/mov/3gp를 복원합니다. GPL이므로 코드는 가져오지 않고 기법만 참고합니다.

### 3.4 AVI

- **[표준]** Microsoft RIFF/AVI: `RIFF....AVI ` 헤더, `LIST hdrl`, `LIST movi`, `idx1`. 스트림 청크는 `00dc`(비디오), `01wb`(오디오) 등입니다.
- 인덱스(`idx1`)가 깨졌어도 `movi` 청크를 순서대로 따라가면 프레임을 얻을 수 있습니다.

### 3.5 CCTV DVR/NVR 독자 포맷

#### Hikvision
- **[확인]** Han, Jeong, Lee, "Analysis of the HIKVISION DVR File System", ICDF2C 2015, Springer, [DOI 10.1007/978-3-319-25512-5_13](https://eudl.eu/doi/10.1007/978-3-319-25512-5_13). 초록 확인. 구조 세부(시그니처, 레이아웃)는 본문에 있을 것으로 보이며 아직 읽지 않았습니다.
- **[확인]** 고응규, 이상진, "HIKVISION Filesystem 미할당 영역에서의 효율적인 동영상 복원 방법에 대한 연구", 디지털포렌식연구 14(4), 2020, [DOI 10.22798/KDFS.2020.14.4.337](https://scholar.korea.ac.kr/handle/2021.sw.korea/131307?mode=full). 미할당 영역에서 영상을 복원하는 방법을 다룹니다.
- **[미확인]** 마스터 섹터의 `HIKVISION@HANGZHOU` 시그니처, `HIKBTREE` 인덱스 구조 등은 위 논문 본문으로 확인해야 합니다. 이번에 열어 본 DFRWS 2023 Dragonas 등의 논문은 Hik-Connect 모바일 앱을 다룬 것이어서 파일시스템 내용은 없었습니다.
- 기법(논문 확인 후 확정): ① 인덱스 트리에서 블록별 채널·시작·종료 시각을 읽고 → ② 삭제·덮어쓰기로 인덱스가 빠진 블록은 프레임 헤더를 직접 스캔해서 → ③ 시간순으로 재조립합니다.

#### Dahua
- **[확인]** Rzayeva 외, "Automated Forensic Recovery Methodology for Video Evidence from Hikvision and Dahua DVR/NVR Systems", *Information* 16(11), 983, 2025, [DOI 10.3390/info16110983](https://www.mdpi.com/2078-2489/16/11/983).
  - 대상: Hikvision 독자 프레임 포맷, Dahua DHFS4.1
  - 기법: 적응형 시간 간격(gap) 탐지, DHFS 프레임 헤더·푸터 이중 시그니처 검증, 512/1024/2048 바이트 오프셋의 시그니처로 제조사 자동 판별
  - 결과(논문 자체 보고): 하드디스크 27개에서 복구율 91.8%, 오탐률 2.4%. 암호화 디스크는 43%에 그쳤습니다.
- **[미확인]** DHAV 프레임 헤더가 `DHAV`, 푸터가 `dhav`라는 점은 업로드 exe의 문자열(`DHAV CCTV`)과 SysDev 매뉴얼의 "dhav (DHFS)" 표기와는 맞아떨어집니다. 하지만 바이트 구조는 위 논문 본문으로 확인해야 합니다.

#### Honeywell (참고)
- **[확인]** Yoon, Hwang, "Forensic analysis of video data deletion and recovery in Honeywell surveillance file system", [arXiv:2605.07430](https://arxiv.org/pdf/2605.07430).
  - 대상 기기 HN35080200: GPT 구조, 영상 데이터는 0x80000000부터 시작합니다.
  - 각 H.264 NAL 앞에 20바이트 독자 헤더가 붙습니다(0x82=IDR, 0x02=non-IDR).
  - 포맷, 만료 삭제, 덮어쓰기 세 경우 모두 영상 영역의 원본 데이터가 남아 있어 복구에 성공했습니다. 단, 시험한 기기는 한 모델뿐입니다.
- **시사점**: DVR의 "포맷"과 "삭제"는 대개 메타데이터만 지웁니다. 이것이 프레임 직접 스캔 방식이 강한 이유입니다.

### 3.6 재포장(출력)

- 복구한 프레임은 내용을 바꾸지 않고 MP4(또는 AVI)로 감쌉니다. 증거 무결성을 위해 원본 오프셋과 해시를 함께 기록합니다.
- 출력 파일명 예: `CH{채널}_{시작시각}_{원본오프셋}.mp4`
- 함께 만드는 리포트: CSV/JSON 형식으로 채널, 시작·종료 시각, 프레임 수, 원본 섹터 범위, SHA-256

---

## 4. 아키텍처와 언어 선택

### 4.1 언어: **Rust (권장)**
- 수 TB 디스크를 순차 스캔해야 하므로 C/C++ 수준의 속도가 필요합니다.
- 손상된 입력을 파싱하는 일이 대부분이라, 메모리 안전성이 크래시와 보안 문제를 줄여 줍니다.
- Windows 단일 exe로 배포하기 쉽습니다. 참고 제품도 Windows GUI입니다.
- 대안: Python으로 시제품을 빠르게 만들 수 있지만, 대용량 스캔 성능 때문에 결국 다시 써야 할 가능성이 큽니다. 포맷 조사용 보조 스크립트에는 Python을 씁니다.

### 4.2 모듈 구조

```
videorec/
├─ crates/
│  ├─ io/         # 읽기 전용 소스: 원시 이미지(.dd/.img), 물리 디스크, (후속) E01
│  ├─ fs/         # FAT32, exFAT 파서 (삭제 엔트리 포함), (후속) NTFS
│  ├─ scan/       # 시그니처 스캐너: Annex B NAL, ISO BMFF 박스, RIFF, DHAV, HIK …
│  ├─ codec/      # SPS/PPS/슬라이스 헤더 파서 (검증용, 디코딩 안 함)
│  ├─ vendor/     # 제조사별 플러그인: hikvision, dahua, honeywell …
│  ├─ assemble/   # 프레임 → 스트림 묶기 (채널, 시간 연속성, gap 탐지)
│  ├─ mux/        # MP4/AVI 작성기 (자체 구현, 외부 GPL 코드 미사용)
│  └─ report/     # CSV/JSON/해시 리포트
├─ cli/           # 1단계: 명령줄 도구
└─ gui/           # 2단계: 데스크톱 GUI (Tauri 또는 egui, 추후 결정)
```

- **원본은 절대 쓰지 않음**: 소스는 읽기 전용으로만 엽니다. 결과물은 다른 드라이브에 씁니다.
- **플러그인 구조**: 장치 포맷이 수십 종이므로 `vendor` 모듈마다 `detect() → scan() → frames()` 인터페이스를 맞춥니다.
- **미리보기**: GUI 단계에서 ffmpeg(LGPL 빌드)로 디코딩합니다. 라이선스 조건은 배포 전에 검토합니다.

---

## 5. 테스트 방법

### 5.1 합성 테스트 이미지 (정답을 아는 데이터)
1. ffmpeg로 프레임마다 번호와 시각이 찍힌 H.264/H.265 테스트 영상을 만듭니다.
2. `mkfs.vfat` / `mkfs.exfat`로 만든 이미지 파일에 영상 여러 개를 복사합니다.
3. 시나리오별 이미지를 만듭니다: 일반 삭제, 빠른 포맷, 부분 덮어쓰기, 조각화(파일 두 개를 번갈아 기록), 녹화 중 전원 차단(moov 없음).
4. 정답 프레임 목록(오프셋, 해시)을 함께 저장합니다.

### 5.2 실제 장치 이미지
- 실제 블랙박스 SD카드와 DVR 하드디스크를 섹터 단위로 이미징합니다(dd, FTK Imager 등). 쓰기 방지 장치 사용을 권장합니다.
- 테스트용 장치에 녹화 → 삭제·포맷 → 이미징을 반복해 시나리오별 샘플을 쌓습니다.
- 개인정보가 담긴 실제 사건 영상은 저장소에 올리지 않습니다.

### 5.3 평가 지표
- 프레임 복구율 = 복구된 정상 프레임 ÷ 정답 프레임
- 재생 가능 파일 비율 (`ffmpeg -v error`로 디코딩 오류 0인 파일)
- 오탐률, 시각·채널 정확도, 1TB당 스캔 시간
- **블랙박스 벤치마크**: 같은 이미지를 참고 제품 두 개에 돌려 같은 지표로 비교합니다. 입력과 출력만 비교하고 내부는 보지 않습니다.

---

## 6. MVP 제안

**범위: 블랙박스 SD카드 (FAT32/exFAT) + 범용 H.264/H.265 카빙, 명령줄 도구**

블랙박스를 먼저 하는 이유:
- 공개 규격만으로 구현할 수 있습니다(FAT/exFAT, ISO BMFF, H.264/H.265).
- 합성 테스트 이미지를 쉽게 만들어 정답과 비교할 수 있습니다.
- 여기서 만드는 NAL 카빙, 재조립, 재포장 모듈을 CCTV 단계에서 그대로 다시 씁니다.

MVP 기능:
1. 디스크 이미지(.img/.dd)를 읽기 전용으로 엽니다.
2. FAT32/exFAT에서 삭제된 엔트리를 찾아 연속 할당을 가정하고 복구합니다.
3. MP4/MOV/AVI를 시그니처로 카빙합니다(포맷 후 복구).
4. moov가 없는 MP4는 참조 파일로 샘플 테이블을 다시 만듭니다.
5. 원시 H.264/H.265 NAL을 카빙하고 MP4로 재포장합니다.
6. CSV/JSON 리포트(원본 오프셋, 해시)를 남깁니다.
7. 합성 테스트 이미지 생성 스크립트와 자동 평가 스크립트를 포함합니다.

MVP 다음 단계: ② Hikvision → ③ Dahua → ④ GUI → ⑤ 기타 제조사

### 진행 전에 필요한 것
- 실제 사용 중인 블랙박스·DVR 모델명과, 테스트용으로 쓸 수 있는 SD카드·디스크 이미지
- Hikvision/Dahua 논문 원문 확보(유료 논문일 수 있음) **[미확인: 접근 가능 여부]**

---

## 7. 참고 자료

- SysDev Laboratories, myCCTV Recovery 매뉴얼 — https://www.sysdevlabs.com/manuals/mycctv/software-overview-and-main-features.php
- SysDev Laboratories, myCCTV Recovery EULA — https://www.sysdevlabs.com/licenses/mycctv_recovery_eula.pdf
- Han, Jeong, Lee (2015), Analysis of the HIKVISION DVR File System, ICDF2C — https://eudl.eu/doi/10.1007/978-3-319-25512-5_13
- 고응규, 이상진 (2020), 디지털포렌식연구 14(4) — https://scholar.korea.ac.kr/handle/2021.sw.korea/131307?mode=full
- Rzayeva 외 (2025), Information 16(11) 983 — https://www.mdpi.com/2078-2489/16/11/983
- Yoon, Hwang, Honeywell surveillance FS, arXiv:2605.07430 — https://arxiv.org/pdf/2605.07430
- ccbrown/teslacam-recovery — https://github.com/ccbrown/teslacam-recovery
- anthwlock/untrunc (GPL-2.0) — https://github.com/anthwlock/untrunc
- [표준] ITU-T H.264, ITU-T H.265, ISO/IEC 14496-12, ISO/IEC 14496-15, Microsoft FAT32 File System Specification, Microsoft exFAT File System Specification, Microsoft RIFF/AVI 문서 (이번 작업에서 원문 미확인)
