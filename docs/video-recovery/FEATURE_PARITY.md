# myCCTV Recovery 공개 기능 대비 videorec 지원 현황

작성: 2026-11-10. 기준: SysDev Laboratories 공개 웹 페이지 (아래 출처). 프로그램 자체는 분석하지 않았습니다 (EULA 역설계 금지).

## 업데이트 내역에 대해

- **공개된 변경 기록(changelog)은 없습니다.** 사이트 맵에 변경 기록 페이지가 있는 것은 UFS Explorer, Recovery Explorer, Raise Data Recovery뿐입니다. ([사이트 맵](https://www.sysdevlabs.com/map.php))
- 현재 버전은 **3.30, 2026-10-29 업데이트**입니다. ([제품 페이지](https://www.sysdevlabs.com/product.php?id=mcctv))
- 정식님 PC의 cctvrec-kr.exe는 버전 리소스상 3.27입니다. 3.27과 3.30 사이에 무엇이 바뀌었는지는 **알 수 없습니다**.
- [미확인] 소프트웨어 배포 사이트에 3.17(2024-04), 3.29.1(2026-09-10) 버전 표기가 있다는 조사 결과가 있지만, 제가 직접 확인하지는 못했습니다.

## 비교표

상태: ✅ 지원 · 🟡 일부 · ❌ 아직 없음 · ➖ 해당 없음

| 분류 | myCCTV Recovery (공개 설명) | videorec | 비고 |
|---|---|---|---|
| 저장장치 | 디스크, USB, SD카드, 디스크 이미지 | 🟡 | 이미지 파일만. 실제 장치는 `\\.\PhysicalDriveN` 경로 지원 여부 미확인 |
| 저장장치 | 파티션 하나만 또는 전체 스캔 | 🟡 | 모든 볼륨 자동 처리, 하나만 고르기는 없음 |
| 읽기 전용 | 원본에 쓰지 않음 | ✅ | |
| 스캔 필터 | 날짜·시간 범위 | ✅ | `--from`, `--to` (녹화기 시각이 있는 형식만: 현재 G-NET .jdr) |
| 스캔 필터 | 카메라 번호 (1,3-5) | ✅ | `--camera` (같은 범위) |
| 진행 표시 | 진행률, 경과·남은 시간 | 🟡 | .jdr 스캔 단계에 표시. 다른 단계는 아직 |
| 진행 표시 | 전체 스캔 건너뛰기 | ❌ | 중간에 멈추고 찾은 것 유지 |
| 세션 | 스캔 결과 저장, 나중에 다시 열기 (.mrse) | ✅ | `recover --save-session s.json` 후 `export s.json -o 폴더 [거르기·형식]`. 위치만 저장(영상 없음), 원본이 같은지 크기·앞뒤 1MiB 해시로 확인. 현재 .jdr 결과만 |
| 세션 | 스캔 결과 ID, 장치 정보 | 🟡 | report.json에 원본 크기·볼륨·스캔 범위 기록 |
| 결과 보기 | 카메라별·날짜별·폴더별 | 🟡 | `--group camera / date / camera-date` 폴더 정리. 목록 화면(GUI)은 없음 |
| 결과 보기 | 미리보기, 재생기, 정렬·검색·선택 | ❌ | GUI 필요. report.csv로 대체 중 |
| 출력 | AVI | ✅ | `--format avi` (OpenDML로 1GB 이상 가능). 기본은 MP4 |
| 출력 | 원래 프레임을 바꾸지 않음 | ✅ | 프레임 바이트 그대로 재포장, 원본과 디코딩 결과 동일 확인 |
| 영상 코덱 | H.264, H.265 | ✅ | |
| 영상 코덱 | Motion-JPEG | ✅ | 이번에 추가. 프레임 속도는 모르면 10fps로 가정 (`--fps`) |
| 영상 코덱 | MJPEG2000, MPEG-4 Part 2, MPEG-1/2 | ❌ | |
| 오디오 | PCM, ADPCM, G.711 | ✅ (.jdr PCM) | .jdr AD 청크 = 16비트 PCM 8kHz 모노(실제 카드로 확인), MP4/AVI에 넣음. ADPCM·G.711은 표본이 없어 미구현 |
| 파일시스템 | FAT/FAT32, exFAT | ✅ | 삭제 파일 복구 포함 |
| 파일시스템 | NTFS | ✅ | MFT의 삭제 레코드에서 파일 복구, 클러스터 비트맵으로 빈 공간 판단. GPT 파티션도 인식 |
| 파일시스템 | HFS+, APFS, ext2/3/4, XFS, JFS, ReiserFS, Btrfs | ❌ | 파일시스템이 없어도 프레임 카빙은 동작 |
| CCTV 파일시스템 | WFS, DHFS(Dahua), HIK(Hikvision), Mirage, Pinetron, BKFL | ❌ | 공개 논문이 있는 HIK, DHFS, WFS부터 검토 예정 |
| 녹화기 스트림 형식 | dhav, dch264, hav, idis, ndav 등 30여 종 | ❌ | 파일시스템 없이도 내부 H.264/H.265는 일반 카빙으로 일부 복구 가능 |
| 블랙박스 형식 | (myCCTV 목록에는 없음) G-NET .jdr | ✅ | 링 덮어쓰기 영역의 옛 조각 포함 |
| 브랜드 | 30개 (Hikvision, Dahua, IDIS, Samsung, LG 등) | ❌ | 위 형식 지원과 함께 늘어남 |
| 라이선스 | 체험판 768KB 제한 등 | ➖ | |

## 다음 순서 (제안)

1. 진행률을 모든 단계로 확장, 중간 중단 시 찾은 것 저장
2. ~~.jdr 오디오~~ 완료 (2026-11-10)
3. CCTV 파일시스템: 공개 연구 자료가 있는 것부터 (HIK, DHFS, WFS). 실제 장치 이미지가 있어야 검증 가능
4. ext4
5. MPEG-4 Part 2 카빙

## 출처

- https://www.sysdevlabs.com/product.php?id=mcctv (버전 3.30, 지원 파일시스템·코덱·형식·요구 사양)
- https://www.sysdevlabs.com/line-mcctv.php (기능, 브랜드 30개, 라이선스)
- https://www.sysdevlabs.com/manuals/mycctv/ (매뉴얼: 저장장치 선택, 스캔 필터, 세션 저장, 결과 보기, 저장)
- https://www.sysdevlabs.com/map.php (변경 기록 페이지가 있는 제품 목록)
