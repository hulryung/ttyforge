# ttyforge — 개발 계획

> 가상 시리얼 포트를 "벼려내는" 도구. socat/ser2net 조합을 시리얼 작업에
> 특화된 단일 Rust 바이너리로 대체한다. python(pyserial)·minicom·tio 등
> 어떤 도구든 진짜 `/dev/tty*` 를 여는 것과 똑같이 사용한다.

## 1. 왜 만드는가 (serial-tether 와의 관계)

serial-tether 는 **실제 장비 한 대를 여러 클라이언트가 공유**하는 데몬이고,
`tether pty` 로 실제 포트를 가상 tty 로 노출하는 기능은 이미 있다.
ttyforge 는 그 반대편 절반을 채운다 — **실제 장비가 없거나, 장비 대신
가짜가 필요한** 상황:

| 상황 | 지금 쓰는 것 | ttyforge |
|---|---|---|
| 하드웨어 없이 pyserial/앱 테스트 | `socat pty,raw…` (raw≠rawer 함정) | `pair` |
| CI 에서 장비 응답 시뮬레이션 | 수제 python + socat 조합 | `sim` |
| 원격 장비 포트를 로컬 /dev 처럼 | ser2net + socat | `bridge` |
| 포트 1개 → 도구 N개 (데몬 없이) | tether pty (데몬 필요) | `mux` |

실전 동기: serial-tether 의 zmodem 검증에서 `socat pty,raw` 가 line
discipline 을 완전히 끄지 않아 Bad CRC 로 전송이 깨졌고 (`rawer` 로 해결),
테스트 하네스 전체가 socat 파싱·sleep 폴링으로 지저분했다. ttyforge 는
이 시나리오를 first-class 로 지원한다.

**겹침 정리**: `mux` 는 tether pty 와 기능이 겹친다. 포지셔닝은
"데몬·세션·락·원격이 필요하면 tether, 로컬에서 한 줄로 쪼개기만 하면
ttyforge mux". 두 프로젝트는 상호 참조 문서로 연결한다.

## 2. 확정된 CLI 설계

단일 바이너리, 포그라운드 실행 (수명 = 프로세스 수명, Ctrl-C 로 종료).
준비되면 stdout 에 포트 경로를 한 줄씩 출력·flush (스크립트 캡처용) —
`tether pty` 의 readiness 계약과 동일.

```sh
# M1 — 가상 널모뎀 페어
ttyforge pair                                  # /tmp/ttyforge-<pid>-{a,b}.pty 출력
ttyforge pair --link /tmp/a.pty --link /tmp/b.pty

# M2 — 장치 시뮬레이터
ttyforge sim --preset echo                     # 에코 장치
ttyforge sim --preset uboot                    # '=> ' 프롬프트, printenv 스텁
ttyforge sim --link /tmp/board.pty -- python3 fake_board.py
#   └ 자식 stdin = 포트에 쓰인 바이트, 자식 stdout = 포트로 나갈 바이트

# M3 — 와이어 모델 (모든 모드 공통 플래그)
ttyforge pair --baud-sim 115200 --latency 5ms --drop 0.001 --seed 42

# M4 — 네트워크 브리지
ttyforge bridge tcp://lab-host:5557            # 원격 포트 → 로컬 가상 tty
ttyforge bridge listen://:7000 --link /tmp/remote.pty

# M5 — 포트 멀티플렉서
ttyforge mux /dev/ttyUSB0 -b 115200 --link /tmp/mon.pty --link /tmp/script.pty
```

## 3. 아키텍처 — tether 에서 가져오는 것

```
src/main.rs        clap 트리 + 런타임 (완료)
src/forge/pty.rs   가상 포트 코어  ← tether.rs create_client_pty / tetherd/pty.rs
src/forge/wire.rs  타이밍·결함 레이어 (신규)
src/forge/pair.rs  pty ↔ wire ↔ pty
src/forge/sim.rs   pty ↔ wire ↔ (자식 프로세스 stdio | 내장 preset)
src/forge/bridge.rs pty ↔ wire ↔ TcpStream
src/forge/mux.rs   ring buffer fan-out  ← tetherd/buffer.rs 패턴
```

tether 에서 검증된 채로 이식하는 규칙 4가지 (`src/forge/pty.rs` 주석에도 기록):

1. **slave 에 완전한 `cfmakeraw`** — echo=0 만으로는 부족. IXON 이
   0x11/0x13 을 삼키고 ICRNL/ONLCR 이 CR/NL 을 변형해 바이너리가 깨진다
   (zmodem Bad CRC 사건의 원인). → `tetherd/pty.rs` 의 termios 설정 이식.
2. **slave fd 를 프로세스가 계속 쥔다** — 소비자(minicom)가 포트를
   닫았다 열어도 master 가 EIO 로 죽지 않게. → `tether.rs`
   `create_client_pty` 의 slave_keep 패턴.
3. **master 는 O_NONBLOCK + tokio AsyncFd** + 수동 libc read/write 루프.
   → `tetherd/serial.rs` FdPort (read/write_all) 패턴.
4. **symlink 로 공개, RAII 로 정리** — 안정된 경로 제공, 모든 종료
   경로에서 링크 제거. → `PtyLinkGuard` 패턴.

`sim` 의 exec 백엔드는 방금 만든 `tether zmodem` 의 lrzsz 브리지 구조
(자식 stdio 양방향 펌프 + kill_on_drop)를 그대로 재사용한다. 방향만
반대다: tether 에서 자식은 포트의 *소비자*, ttyforge 에서 자식은 *장치*.

## 4. 마일스톤

각 단계는 "수용 기준"을 통과해야 다음으로 넘어간다.

### M0 — 스캐폴드 ✅ (완료)
clap 서브커맨드 트리, 모듈 배치, 빌드 통과. 각 모듈 헤더에 설계 근거 기록.

### M1 — pty 코어 + `pair`
- `VirtualPort` 구조체: create(openpty+cfmakeraw) / read / write_all / 링크 관리
- `pair`: 양방향 펌프, 준비 시 경로 2줄 출력
- **수용 기준**: ① pyserial ↔ minicom 상호 대화 ② serial-tether 의
  zmodem 루프백 테스트에서 socat 을 `ttyforge pair` 로 교체해
  200 KB 양방향 바이트 일치 (dogfood — 이게 진짜 시험대)
- 테스트: termios 플래그 단위 검증 + 바이너리 전체 값(0x00–0xFF) 왕복

### M2 — `sim`
- exec 백엔드 (자식 stdio 펌프) + preset 4종: echo / shell / uboot / at
- preset `shell`·`uboot` 는 serial-tether 의 `exec`/`run` 통합 테스트가
  실장비 없이 돌아갈 수준의 최소 구현 (tether 테스트 인프라로도 제공 가능)
- **수용 기준**: `tether -D $(ttyforge sim --preset uboot)` 로
  `tether exec "printenv"` 성공

### M3 — `wire` (차별화 포인트)
- `--baud-sim`(스루풋 스로틀) / `--latency` / `--jitter` / `--drop` /
  `--corrupt`, `--seed` 로 재현 가능
- 모든 모드에 공통 적용 (전역 플래그)
- **수용 기준**: `--baud-sim 9600` 에서 1 KB 전송이 ≈1.04초,
  `--drop` 하에서 zmodem 이 재전송으로 살아남는 것을 시연

### M4 — `bridge`
- raw TCP (connect/listen, 피어 끊겨도 포트 유지·재수락)
- M4b: RFC2217 클라이언트 (가상 포트의 termios 변경 → COM-PORT-OPTION),
  ser2net 상호운용 테스트
- **수용 기준**: `tetherd --tcp` ↔ `ttyforge bridge tcp://…` 로 원격
  장비에 minicom 접속

### M5 — `mux`
- ring buffer + 소비자별 cursor (tetherd/buffer.rs 패턴), TX 직렬화
- **수용 기준**: 소비자 2개가 각각 RX 전체 사본을 수신 (tether 가 측정한
  128/72 분할 문제의 부재를 테스트로 고정)

### M6 — 배포·마감
- `--json` 상태 출력, README/예제, GitHub Actions CI (macOS+Linux),
  homebrew-tap 등록 (기존 ~/dev/homebrew-tap 재사용), crates.io publish
  (`ttyforge` 이름 확보 확인됨)

## 5. 플랫폼·제약 (미리 못박는 것)

- **unix 전용** (macOS + Linux). Windows 가상 COM 은 com0com/커널 드라이버
  영역이라 범위 밖 — README 에 명시.
- pty 에는 UART 가 없다: 소비자가 가상 포트에 건 baud 변경은 no-op
  (M3 `--baud-sim` 이 시뮬레이션 담당), DTR/RTS 는 전달 불가.
  tether 문서와 동일한 제약 — 동일 문구로 문서화.
- 에디션 2021 (tether 코드 이식 마찰 최소화), rust 1.85+.

## 6. 리스크 / 열어둔 결정

- **RFC2217 범위**: termios 폴링 vs TIOCPKT 이벤트 — M4 착수 시 결정.
- **preset 확장**: rhai/lua 스크립팅 요구가 생기면 M2 exec 백엔드로
  충분한지 재평가 (일단 "장치는 그냥 프로그램" 철학 유지).
- **crate 분리**: pty 코어가 안정되면 `ttyforge-pty` 라이브러리로 분리해
  serial-tether 가 역으로 의존하는 옵션 (중복 제거). M6 이후 판단.
