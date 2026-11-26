# CCTV 전용 파일시스템: 공개 자료 조사 (2026-11-10)

이 문서는 공개 논문에 나온 내용만 정리했습니다. 논문 원문을 직접 확인하지 못하고 요약본으로만 본 항목은 [미확인]으로 표시했습니다.
아직 실제 DVR/NVR 디스크 이미지로 검증한 항목은 하나도 없습니다.

## 1. Hikvision (HIKVISION@HANGZHOU / HIKBTREE)

출처: Han, Jeong, Lee, "Analysis of the HIKVISION DVR File System", ICDF2C 2015, LNICST 157, pp.189-199,
DOI 10.1007/978-3-319-25512-5_13 (https://eudl.eu/pdf/10.1007/978-3-319-25512-5_13)

- 디스크 구성은 Master Sector, System Logs, Video Data Area, HIKBTREE의 네 영역입니다.
- Master Sector는 offset 0x200에 있고 크기는 256바이트입니다. 서명은 "HIKVISION@HANGZHOU"이고 리틀엔디언입니다. 백업은 시스템 로그 옆에 있습니다.
- 데이터 블록 크기: 예시 값은 0x400000인데, 본문에는 "보통 1GB(0x40000000)"라고 적혀 있어 서로 맞지 않습니다. [미확인]
- 로그 레코드는 "RATS"(52 41 54 53 01 00 00 00)로 시작하고 UNIX 시각을 씁니다. 종류는 0x01 알람, 0x02 예외, 0x03 조작, 0x04 정보입니다.
- 각 데이터 블록에는 H.264 데이터가 들어 있고, 블록 끝에 IDR 테이블이 있습니다. 항목은 "OFNI"로 시작하는 56바이트이며 뒤에서 앞으로 쌓입니다.
- HIKBTREE는 서명이 "HIKBTREE"이고 페이지가 4KB이며 백업 트리가 있습니다. 항목에 들어 있는 것:
  - 사용 여부 (0x00 사용 중, 0xFF 빔)
  - 채널
  - 시작/끝 시각 (기록이 끝나지 않았으면 FF FF FF 7F 00 00 00 00)
  - 데이터 블록 offset
- 삭제 기능은 없고, 영상을 지우는 방법은 초기화뿐입니다. 초기화하면 로그를 0으로 지우고 HIKBTREE를 다시 만들지만, 영상 데이터는 남아 있어서 카빙할 수 있습니다.
- 바이트 단위 항목 offset은 이번에 읽은 범위에 없었습니다. [미확인]

## 2. Dahua DHFS4.1 (DHAV 프레임)

출처: Rzayeva 외, "Automated Forensic Recovery Methodology for Video Evidence from Hikvision and Dahua DVR/NVR Systems",
Information (MDPI) 16(11):983, 2025-11-13, DOI 10.3390/info16110983 (https://www.mdpi.com/2078-2489/16/11/983)

- 서명 "DHFS4.1"을 offset 512, 1024, 2048에서 찾습니다. [미확인: 요약본]
- 프레임 머리는 "DHAV", 꼬리는 "dhav"입니다. 이 두 서명과 크기를 함께 검증합니다. 이 부분은 원문을 직접 읽어 확인했습니다.
- 머리에 들어 있는 것: type/subtype, channel/subchannel, 프레임 번호(u32), 프레임 크기(u32), 날짜시각(u32), ms(u16). 각 필드의 offset은 나와 있지 않습니다. [미확인]
- 같은 논문에 Hikvision 프레임 머리 필드도 나오지만 서로 맞지 않는 값이 있습니다. [미확인]
- 복구 방법: 프레임을 시각순으로 정렬하고, 간격이 기대값의 2배를 넘으면 끊습니다. 10프레임 이상만 남기고, ffmpeg `-c:v copy`로 MP4를 만듭니다. [미확인: 요약본]
- 삭제가 어떻게 동작하는지는 설명이 없습니다.

## 3. Honeywell NVR (참고, 국내 연구)

출처: Yoon, Hwang(성균관대), "Forensic analysis of video data deletion and recovery in Honeywell surveillance file system",
DFRWS USA 2026 camera-ready (https://dfrws.org/wp-content/uploads/2026/04/DFRWS_USA_2026_Camera_Ready_Paper_16.pdf, arXiv 2605.07430)

- 디스크는 GPT이고, Partition 1이 영상 저장소입니다. 영상은 0x80000000부터 시작합니다.
- NAL마다 앞에 20바이트 머리가 붙습니다.
  - byte0: 0x82는 IDR, 0x02는 non-IDR
  - byte1-3: 80 01 00
  - byte4-7: 가로·세로 (u16 LE)
  - byte8-11: NAL 길이
  - byte12-19: UNIX 시각, 마이크로초 단위 (u64)
- 채널 데이터 끝은 0으로 채운 20바이트로 표시합니다.
- 보존 기간이 지나 만료되거나 덮어쓰면 색인만 지워지고 영상 바이트는 남습니다. 블록 그룹 시작 시각보다 이른 시각이 찍힌 프레임은 삭제된 영상일 가능성이 높습니다.
- 일부 예시 값은 엔디언이 본문 설명과 맞지 않습니다. [미확인]

## 4. WFS 0.4

이번 조사에서는 구조를 설명한 공개 논문을 찾지 못했습니다. 검색에서 나온 것은 포럼 글뿐이고 출처로 쓸 수 없습니다.
그래서 지금은 모릅니다.

## videorec에 반영할 계획

1. 지금도 있는 H.264/H.265 카빙만으로 위 장비들의 영상 데이터를 찾을 수 있습니다. 각 논문 모두 영상 바이트가 그대로 남는다고 적고 있습니다.
2. 바로 구현할 수 있는 것
   - Dahua DHAV/dhav 머리·꼬리 검증
   - Honeywell 20바이트 머리 파싱
   - Hikvision Master Sector와 HIKBTREE 서명 감지
   다만 필드 offset이 [미확인]인 항목이 많아서, 실제 장비 디스크 이미지로 검증하기 전에는 결과를 "추정"으로 표시해야 합니다.
3. 필요한 것: 각 장비의 디스크 이미지 1개 이상. 이미지가 없으면 구현해도 검증할 수 없습니다.
