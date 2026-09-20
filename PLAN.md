## Filen Storage Optimizer — ACTIVE

Goal: Filen 클라우드 파일을 무손실(이미지/오디오) 및 측정 기반 시각적 무손실(영상)로 재인코딩해 사용 용량을 줄이는 Rust CLI
Started: 2026-09-19
Full design: `~/.claude/plans/filen-graceful-gadget.md`

### Steps
- [x] 1. 스켈레톤 — Cargo.toml, `cli.rs`, `config.rs`, `preflight.rs` (44 tests green)
- [x] 2. Remote + 인벤토리 — `remote/{mod,rcd,rclone_cli}.rs`, `ledger.rs`, `scan` (81 tests green)
- [ ] 3. 분류 + 정책 + `plan`
- [ ] 4. 이미지·오디오 변환 + 검증 (로컬)
- [ ] 5. 스테이징 + `run` 파이프라인 (dry-run)
- [ ] 6. 쓰기 경로 + 휴지통 정책 + `cleanup`
- [ ] 7. 영상 티어 — `vmaf.rs`, `convert/video_av1.rs`, `bench`
- [ ] 8. dedup / restore
- [ ] 9. 커밋

### Key constraints
- 전송은 rclone `filen` 백엔드 (1.73+ Tier 1). 공식 `filen-sdk-rs`는 crates.io 미배포라 미사용
- 인코딩은 직접 구현하지 않고 `cjxl`/`djxl`, `flac`, `ffmpeg`, `ab-av1` 서브프로세스 호출
- 로컬 디스크 제한 → 디스크 예산 세마포어 기반 스테이징. 전체 싱크 금지
- 원본 삭제는 업로드본 크기+blake3 확인 통과 후에만
- Filen 휴지통은 할당량에 포함. `trash_policy` 기본 `keep` (테스트 중 복구 가능)
- 영상: SVT-AV1 소프트웨어 (NVENC 금지, 같은 품질에서 30~45% 큼). GPU는 NVDEC만
- 영상 컨테이너는 소스 유지 (MOV→MOV). MKV는 QuickTime keyed atom 슬롯이 없음

### Gotchas found while building (do not regress)
- ffmpeg 기능 탐지는 긍정 마커(`"Encoder libsvtav1"` / `"Filter libvmaf"`)로. 부재 시 출력에도
  이름이 들어가서(`Unknown filter 'libvmaf'.`) `contains(name)` 은 부재를 존재로 오판한다.
- libjxl 고급 플래그는 `-v -v --help` 에만 나온다. 평범한 `--help` 는 37줄짜리 요약이다.
- `djxl -J` (`--reconstruct_jpeg`) 필수. 없으면 복원 불가 JXL에 대해 조용히 새 손실 JPEG를 만든다.
- 세마포어 permit은 MiB 단위. `Semaphore::acquire_many` 가 `u32` 라 바이트로는 4GB에서 터진다.
- SVT-AV1 4.2.0 `--lp` 는 코어 수가 아니라 `[0,6]` 병렬화 레벨. 코어 제한은 `taskset -c`.
- SVT-AV1 4.2.0 `--tune` 기본값이 1(PSNR)이라 VMAF를 부풀린다. `tune=0` 명시. `tune=5`(VMAF) 금지.
- `lsjson`과 RC `operations/list`는 경로 기준이 다르다. lsjson은 나열한 디렉터리 기준,
  operations/list는 `fs` 기준(= `remote` 하위경로가 Path에 이미 포함). 스코프를 두 번 붙이면
  `sub/sub/b.txt` 가 된다. 파리티 테스트가 이걸 잡았다.
- rclone 종료 코드로 부재(3=dir, 4=file)와 실패(1=usage, 2=기타, 5=일시적, 7=치명)를 구분할 것.
  전부 `Ok(None)` 으로 삼키면 네트워크 장애가 "파일 없음"이 되어 원본을 지울 수 있다.
- `rand` 0.10 은 `random_range` 를 `Rng` 에서 `RngExt` 로 옮겼다.
- lib + bin 분리 유지. 바이너리 단독이면 테스트 전용 공개 API가 dead_code 로 잡힌다.

### Status
2단계 완료. 3단계(분류 + 정책 + plan) 대기 중.
