## Filen Storage Optimizer — ACTIVE

Goal: Filen 클라우드 파일을 무손실(이미지/오디오) 및 측정 기반 시각적 무손실(영상)로 재인코딩해 사용 용량을 줄이는 Rust CLI
Started: 2026-09-19
Full design: `~/.claude/plans/filen-graceful-gadget.md`

### Steps
- [x] 1. 스켈레톤 — Cargo.toml, `cli.rs`, `config.rs`, `preflight.rs` (44 tests green)
- [x] 2. Remote + 인벤토리 — `remote/{mod,rcd,rclone_cli}.rs`, `ledger.rs`, `scan` (81 tests green)
- [x] 3. 분류 + 정책 + `plan` — `classify.rs`, `policy.rs`, `report.rs` (133 tests green)
- [x] 4. 이미지·오디오 변환 + 검증 — `convert/{mod,jxl,audio}.rs`, `hash.rs` (152 tests green)
- [x] 5. 스테이징 + 동시성 파이프라인 — `governor.rs`, `staging.rs`, `pipeline.rs` (183 tests green)
- [x] 6. 쓰기 경로 + 휴지통 정책 + `cleanup` + `report` + `scope.rs` (214 tests green)
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
- ffprobe `format_name` 은 단일 값이 아니라 `mov,mp4,m4a,3gp,3g2,mj2` 같은 콤마 목록이다.
- 영상 저비트레이트 게이트는 면적으로 정규화한 bits/pixel/sec 1.0 (= 1080p 2Mbps, 4K 8Mbps).
  0.03 으로 뒀다가 게이트가 아예 발동하지 않는 버그를 테스트가 잡았다.
- `plan` 은 다운로드하지 않으므로 영상은 needs_probe 로 남는다. 추정치를 지어내지 말 것.
- framehash 출력은 헤더 줄이 다를 수 있다 (`#sar 1/1` vs `0/1`). 해시 줄만 비교할 것.
- ffmpeg 에 JPEG XL 디코더가 없는 빌드가 흔하다. `.jxl` 검증은 `djxl` 로 디코딩한 뒤 해시.
- `flac --keep-foreign-metadata` 로 WAV→FLAC 도 바이트 복원 가능 (비용 0.3%). 기본으로 쓸 것.
- `sha2` 0.11 은 해셔에 `io::Write` 를 구현하지 않는다. 청크로 직접 읽어 update.
- 스테이지 큐를 만들지 않았다. 파일당 태스크 + 자원별 세마포어로 같은 중첩이 나온다.
  코어를 기다리는 작업이 네트워크 슬롯을 쥐고 있지 않은 것이 핵심.
- 설정 때문에 생긴 skip(`video_tier_disabled`, `too_large_for_budget`)은 영구 기록하면 안 된다.
  `--allow-video` 를 켜도 아무 일이 안 일어난다. run 시작 시 `reopen_skipped` 로 되돌린다.
- dry-run 은 ledger 를 pending 으로 되돌려야 한다. 안 그러면 실제 실행이 건너뛴다.
- **`--path` 스코프는 claim 쿼리에서 강제해야 한다.** scan 에만 적용하면 run 이 드라이브 전체를
  건드린다 (실제로 `--path /photos` 가 audio/ 를 변환한 버그가 있었다). 접두사는 세그먼트
  경계에 맞춰야 `photos` 가 `photos-backup` 을 삼키지 않는다. LIKE 와일드카드 이스케이프 필수.
- 업로드 확인은 `operations/hashsumfile`(서버측 blake3) + stat 크기. 다운로드 없이 검증된다.
- 클라우드 permit 은 job 종료 후에도 유지(`Lease::hold`). 휴지통 비우기 전까지 여전히 과금된다.

### Status
6단계 완료. 7단계(영상 티어) 대기 중.
