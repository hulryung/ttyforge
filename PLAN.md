# ttyforge — 개발 계획 · 기록

> 가상 시리얼 포트를 "벼려내는" 도구. socat/ser2net 조합을 시리얼 작업에
> 특화된 단일 Rust 바이너리로 대체한다. python(pyserial)·minicom·tio 등
> 어떤 도구든 진짜 `/dev/tty*` 를 여는 것과 똑같이 사용한다.

**상태: M0–M6 전부 완료, v0.1.0 배포됨** — [crates.io](https://crates.io/crates/ttyforge) ·
[릴리스](https://github.com/hulryung/ttyforge/releases/tag/v0.1.0) ·
`brew install hulryung/tap/ttyforge`.
이 문서는 이제 계획서이자 기록이다. 각 마일스톤에는 **무엇을 상대로
검증했는지**와 **무엇이 틀렸는지**가 함께 적혀 있다 — 계획이 실측에 밀려
바뀐 지점이 세 곳 있고(M4 수용 기준, M5 의존성 선택, M4b 플랫폼 도달 범위),
그 셋이 이 문서에서 제일 쓸모 있는 부분이다.

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

## 2. CLI — 구현된 표면

단일 바이너리, 포그라운드 실행 (수명 = 프로세스 수명, Ctrl-C 로 종료).
준비되면 stdout 에 포트 경로를 한 줄씩 출력·flush (스크립트 캡처용) —
`tether pty` 의 readiness 계약과 동일. `--json` 은 그 자리에 한 줄짜리
객체를 대신 내보낸다(forge·pid·ports·wire.seed).

```sh
# 가상 널모뎀 페어
ttyforge pair                                  # /tmp/ttyforge-{a,b}.pty 출력
ttyforge pair --link /tmp/a.pty --link /tmp/b.pty

# 장치 시뮬레이터
ttyforge sim --preset echo|shell|uboot|at
ttyforge sim --link /tmp/board.pty -- python3 -u fake_board.py
#   └ 자식 stdin = 포트에 쓰인 바이트, 자식 stdout = 포트로 나갈 바이트

# 와이어 모델 (모든 forge 공통 전역 플래그)
ttyforge pair --baud-sim 115200 --latency 5ms --drop 0.001 --seed 42

# 네트워크 브리지 (원안의 5557 은 tetherd 포트였다 — §4 M4a 참조)
ttyforge bridge tcp://lab-host:2000            # 원격 raw 포트 → 로컬 가상 tty
ttyforge bridge listen://:7000 --link /tmp/remote.pty
ttyforge bridge --rfc2217 tcp://lab-host:2000  # termios 를 원격 UART 로 중계

# 포트 멀티플렉서
ttyforge mux /dev/ttyUSB0 -b 115200 --link /tmp/mon.pty --link /tmp/script.pty

# 기계 판독 readiness (모든 forge 공통)
ttyforge pair --json --drop 0.001
# {"forge":"pair","pid":4711,"ports":["…a.pty","…b.pty"],"wire":{"drop":0.001,"seed":…}}
```

## 3. 아키텍처 — tether 에서 가져오는 것

```
src/main.rs        clap 트리 + 런타임 (완료)
src/forge/pty.rs   가상 포트 코어  ← tether.rs create_client_pty / tetherd/pty.rs
src/forge/wire.rs  타이밍·결함 레이어 (신규)
src/forge/pair.rs  pty ↔ wire ↔ pty
src/forge/sim.rs   pty ↔ wire ↔ (자식 프로세스 stdio | 내장 preset)
src/forge/bridge.rs pty ↔ wire ↔ TcpStream (raw | RFC2217)
src/forge/rfc2217.rs telnet COM-PORT-OPTION 코덱 + termios 매핑 (M4b)
src/forge/mux.rs   ring buffer fan-out  ← tetherd/buffer.rs 패턴
src/forge/serial.rs 실제 시리얼 포트  ← tetherd/serial.rs FdPort (M5)
src/forge/signals.rs readiness 이전에 등록하는 종료 시그널 (M5)
src/forge/status.rs readiness 발표 — 평문 경로 또는 --json (M6)
scripts/acceptance/  제3자 구현 상대 상호운용 검증 (M6, 수동/workflow_dispatch)
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

### M1 — pty 코어 + `pair` ✅ (완료)
- `VirtualPort`(openpty+cfmakeraw, AsyncFd master, slave-keep) + `Link`
  (symlink + `.pid` sidecar, stale reclaim, RAII 정리) + 방향별 독립 `pump`
- 이식 중 발견한 **규칙 5** 추가: 소비자가 slave 를 cooked 로 바꾸면
  500ms tick 에서 raw 재적용 (tether `reassert_raw_if_needed` 포트)
- tether 데몬 브리지와의 의도적 차이: write timeout/drop 없음 — 페어는
  방향당 피어가 하나뿐이라 커널 버퍼 backpressure 가 곧 올바른 와이어
  동작 (데이터 무손실)
- **수용 기준 통과**:
  ① pyserial↔pyserial 1KB 전 바이트값 양방향 + reopen + baud no-op
    (minicom 미설치로 pyserial 양단 대체)
  ② zmodem 루프백에서 socat → `ttyforge pair` 교체, 200 KB 양방향
    바이트 일치 (tetherd 가 한쪽을 tokio-serial 로 open — 그 경로까지 검증)
- 테스트 7개: termios 플래그 고정, cooked 복구, sidecar 순수 로직,
  링크 정리, 256값 왕복(유닛) + 실바이너리 SIGTERM 정리·reopen 생존(통합)

### M2 — `sim` ✅ (완료)
- exec 백엔드 (자식 stdio 펌프, `$TTYFORGE_PTY` env, 종료코드 통과) +
  preset 4종: echo / shell / uboot / at
- 라인 디시플린(`LineDevice`): 입력 echo, CR/LF/CRLF 통일, backspace,
  Ctrl-C — 실제 펌웨어 콘솔 동작 재현
- 미니 hush/POSIX 셸: tether exec 래퍼가 요구하는 정확한 부분집합 —
  `;` 체인, **인접 따옴표 연결**(`"BE""G"` → `BEG`, echo-split 트릭),
  실행 시점 `$?` 확장, `$VAR` env 확장. uboot 는 printenv/setenv/
  version/help + 기본 env(baudrate=115200 등), posix 는 not-found=127
- **수용 기준 통과** (실제 tether 스택 전체 경유):
  `tether -D <sim> exec "printenv baudrate"` → exit 0, 값 일치 /
  `exec false` → exit 1 미러링 / unknown → exit 1 + U-Boot 오류 문구 /
  `shell=uboot,prompt==> ` personality 로 `run "version"` 성공 /
  `--json` 형태 (exit_code=0, output) 검증
- 보너스 데모: python 가짜 센서(`-- python3 -u …`) ↔ pyserial 소비자
  SCPI 풍 대화 성공
- 테스트 20개 (유닛 14: 토크나이저/래퍼/디시플린 + 통합 6)

### M3 — `wire` ✅ (완료, 차별화 포인트)
- 전역 플래그 6종: `--baud-sim`(8N1: baud/10 B/s) / `--latency` /
  `--jitter` / `--drop` / `--corrupt` / `--seed`. pair 양방향 + sim
  양쪽 백엔드(입·출력 방향별 독립 Wire)에 적용
- 설계: `Wire::plan()` 이 변형 + **배달 스케줄**(셀+만기시각)만 계산,
  IO 는 호출자 담당 — tokio paused-clock 으로 페이싱 수학을 정확히
  유닛 테스트. RNG 는 내장 xorshift64\*(splitmix64 시드) — `rand`
  의존성 없이 플랫폼 무관 바이트 단위 재현
- 의미론 테스트로 고정: 드롭/손상된 바이트도 라인 타임 소비(노이즈가
  와이어를 빠르게 하지 않음), 유휴 라인은 크레딧 적립 없음, 스로틀 시
  ~5ms 단위 셀 배달(버스트 아님), 순서 보존 backpressure. 확률
  피처 사용 시 시드 자동 생성·stderr 공지(CI 실패 재현 보장)
- **수용 기준 통과**:
  ① `--baud-sim 9600` 1000B ≈ 1.04초 (paused-clock 정밀 + 실벽시계
    통합 테스트 양쪽)
  ② zmodem 20 KB가 `--drop 0.002 --seed 7` 와이어에서 수신측 오류
    이벤트 101건을 전부 재전송으로 복구, 바이트 일치
- 테스트 30개 (wire 유닛 8: 스로틀 정밀·유휴 무크레딧·지연·시드
  재현·드롭률·1비트 손상·항등·파서 + 통합 2: 실벽시계 페이싱·시드 드롭)
- 배운 것: pty 커널 버퍼(~2-3KB)를 넘는 순차 write-then-read 테스트는
  backpressure 데드락 — writer 를 스레드로 분리해야 함

### M4a — `bridge` (raw TCP) ✅ (완료)
- `tcp://HOST:PORT` 다이얼, `listen://[HOST]:PORT` 수락. 방향별 독립 태스크
- **포트가 모든 피어보다 오래 산다**: 피어가 끊겨도 pty 는 그대로 —
  listen 은 재수락, tcp 는 250ms→5s 백오프로 재다이얼. minicom 을 붙여둔
  채 랩 호스트를 재부팅해도 경로가 유지된다
- 피어 없는 동안 포트에 쓰인 바이트는 **버린다**(down wire). pair 는 커널
  버퍼에 붙잡아 두지만(양단이 프로세스 시작부터 존재 → 대기가 유한),
  TCP 피어 부재는 무기한이라 붙잡으면 ① 로컬 도구의 write 가 영구 블록
  ② 다음 피어에게 몇 분 묵은 키입력이 쏟아진다
- readiness 계약: listen 은 **bind 후에** 경로 출력(경로를 읽자마자
  접속하는 스크립트가 지지 않게), tcp 는 피어를 기다리지 않고 출력
  (부팅 중인 랩 호스트에서도 포트는 존재해야 한다)
- 와이어 이식: 세션마다 `WireSpec::build_pair_nth(n)` — 재접속이 이전
  세션의 드롭을 복제하지 않으면서 `--seed` 재현성은 유지. pty 가 아닌
  sink 용 `wire::deliver_to`(AsyncWrite). TCP_NODELAY 필수 — Nagle 이
  키입력을 뭉치고 와이어의 ~5ms 셀을 버스트로 뭉갠다
- **수용 기준 (원안 수정)**: 원안의 `tetherd --tcp` 는 raw 바이트가 아니라
  **NDJSON/JSON-RPC 2.0 + 토큰 인증**(tether `docs/PROTOCOL.md` §1–3)이라
  raw 브리지로는 상호운용 자체가 불가능 — 전제가 틀린 기준이었다.
  실제 raw 서버로 대체 검증(통과):
  `ttyforge sim --preset uboot`(원격 보드) → `socat TCP-LISTEN:…
  FILE:<pty>,rawer`(ser2net 역할) → `ttyforge bridge tcp://…` →
  pyserial(minicom 대역)에서 version / printenv / setenv 왕복 성공
- 곁다리로 고친 것: **exit 3 (setup error) 매핑이 처음부터 동작하지
  않았다** — `e.chain().any(|c| c.is::<SetupError>())` 는 체인 프레임의
  구체 타입이 anyhow 의 context 래퍼라 절대 매치되지 않아 모든 셋업 실패가
  2 로 나갔다. `e.downcast_ref::<SetupError>()` 로 교체. 겸사겸사
  `Link::claim` 이 자기 실패를 전부 SetupError 로 태그하게 하고 호출부의
  중복 태그를 제거(“setup failed: setup failed:” 중복 출력도 사라짐)
- 테스트 10개 (유닛 4: 엔드포인트 파싱·오입력 거부·백오프 상한 + 세션별
  와이어 재현성 / 통합 6: 다이얼 바이트 투명성·SIGTERM 정리, listen 재수락
  3회, 원격이 사라졌다 돌아올 때 재다이얼, 피어 없이 32KB 논블록 write,
  `--baud-sim 9600` 페이싱이 브리지에도 적용, 셋업 실패 exit 3 + 준비 라인
  없음)
- 배운 것 ①: 완료된 `JoinHandle` 을 다시 폴하면 패닉("JoinHandle polled
  after completion"). `select!` 로 한 방향이 끝나면 *다른* 쪽만 await 해야
  한다 — 첫 피어 끊김에서 프로세스가 통째로 죽는 것을 통합 테스트가 잡음
- 배운 것 ②: 테스트에서 "빈 포트를 얻고 놓았다가 다시 bind" 하는 패턴은
  병렬 테스트끼리 같은 포트를 받아 `AddrInUse` 로 20% 플레이크. 바인드한
  소켓에서 주소를 가져오거나(`:0`), 실패 시 재시도해야 한다

### M4b — RFC2217 ✅ (완료)
- **결정: termios 폴링(A)** — TIOCPKT(B)는 master 의 read 포맷을 바꿔
  `VirtualPort::read`(모든 forge 공유, 바이너리 투명성의 근거)에 per-read
  접두 바이트 수술을 요구하는데, 그 대가로 사는 것이 지연 단축뿐이다.
  게다가 B 도 "무엇이 바뀌었는지"는 결국 tcgetattr+diff 로 알아내야 하니
  A 의 대체가 아니라 A 위의 최적화. 실측으로 확인한 전제: **macOS pty 는
  baud/CSIZE/PARENB/CSTOPB/CRTSCTS 를 전부 저장**하고(폴링으로 관측 가능),
  **CSIZE 는 무시**한다(CS7 에서도 0xFF 가 그대로 통과 → RFC2217 모드에서
  c_cflag 를 보존해도 바이너리 투명성이 깨지지 않는다)
- 지연 완화: 100ms 폴 + **데이터를 내보내기 직전 동기화**. 실제로 필요한
  보장은 절대 지연이 아니라 순서("baud 변경이 그 뒤 바이트보다 먼저
  도착")이고, 송신과 폴을 한 태스크의 select! 로 합쳐 그 순서를 구조적으로
  보장한다
- `src/forge/rfc2217.rs`: telnet 코덱(IAC 이스케이프/언이스케이프, 옵션
  협상, subneg 파서) + termios→RFC2217 매핑. **speed_t 는 Linux 가 인덱스,
  BSD/macOS 가 숫자 그대로**라 테이블 조회 후 실패 시 원값 사용 (macOS 의
  임의 baud 도 처리)
- rule 5 분리: `reassert_line_discipline_if_needed` — 소비자가 터미널을
  cooked 로 만들면 라인 디시플린만 raw 로 되돌리고 c_cflag(속도/패리티/
  프레이밍)는 보존. 기존 `reassert_raw_if_needed`(8N1 리셋)는 pair/sim 이
  그대로 사용
- 스트림 안전장치: 데이터 셀과 subneg 이 소켓 하나를 공유하므로 writer 를
  Mutex 로 직렬화 — subneg 이 이스케이프된 청크 중간에 끼면 양방향이 깨진다
- **수용 기준 통과** (제3자 구현 상호운용): pyserial 의 `serial.rfc2217.
  PortManager` 서버 대상으로 9600 7O2 적용 확인 → 이후 115200 변경분만
  재전송 확인 → 0xFF 다수 포함 페이로드 바이트 동일 왕복. 덤으로 그 서버가
  독립적으로 재확인해 준 사실: pty 에 `TIOCMGET` 은 ENOTTY (모뎀 라인 부재)
- **범위 밖**: DTR/RTS. 모뎀 라인은 termios 상태가 아니고 pty 에는 없다 —
  소비자의 `TIOCMSET` 을 master 가 관측할 수 없으므로 SET-CONTROL 8..=12 는
  로컬 트리거 자체가 없다 (§5 의 기존 제약과 동일)
- **플랫폼별 도달 범위 (M6 CI 가 밝혀냄)**: forge 는 관측 가능한 것만
  중계할 수 있는데, Linux pty 드라이버는 모든 tcsetattr 를
  `c_cflag &= ~(CSIZE|PARENB); c_cflag |= CS8|CREAD` 로 정규화한다
  (drivers/tty/pty.c). 따라서 **Linux 에서는 데이터 비트·패리티 중계 불가**
  (baud/스톱비트/흐름제어는 가능), macOS 는 5종 전부 가능. 실측 확인:
  Ubuntu 24.04 pty 에 9600 7O2 + RTS/CTS 를 걸면 CS8 / PARENB 없음 /
  CSTOPB 유지 / CRTSCTS 유지 / B9600 유지로 되돌아온다. 통합 테스트는
  요청값이 아니라 **read-back 값**에서 기대치를 도출하도록 수정
- 테스트 13개 (유닛 10: 이스케이프 왕복·모든 경계에서의 분할 디코딩·미지
  옵션 거부와 ack 루프 방지·subneg 파라미터 이스케이프·termios 매핑·변경분만
  전송·발신 명령 자기 디코딩 + rule 5 분리 / 통합 3: 설정 릴레이와 유휴 시
  침묵과 변경분만, 바이너리 투명성, listen 모드 거부 exit 3)

### M5 — `mux` ✅ (완료)
- 팬아웃: 장치를 **읽는 태스크가 정확히 하나**. 그 읽기를 broadcast 채널로
  한 번 게시하고 소비자마다 독립 커서로 사본을 받는다 — tetherd/buffer.rs
  의 ring + per-consumer-cursor 패턴을 tokio 가 이미 구현해 둔 형태. 용량은
  바이트가 아니라 read 개수(BACKLOG=256, 최대 ~2MB)
- 느린 소비자가 장치를 막지 못한다: 밀린 소비자는 **자기 사본만** 오래된
  쪽부터 잃고 그 사실을 stderr 로 통보받는다. UART 까지 backpressure 를 거는
  대안은 아무도 요청하지 않은 데이터 손실을 만든다
- TX 병합: 소비자 N → mpsc → **쓰기 태스크 하나**. 청크 단위 인터리브는
  실제 케이블과 같고, 청크가 쪼개지지는 않는다
- `src/forge/serial.rs` (신규): 실제 포트 열기. **tokio-serial 미사용** —
  FdPort 패턴이 이미 repo 에 있고(pty.rs 규칙 3) tty 를 여는 건 open +
  tcsetattr 인데, tokio-serial 은 mio-serial + serialport(Linux 에서 libudev)
  를 끌고 온다. M6 에서 homebrew/crates.io 로 가는 단일 바이너리가 syscall
  두 개 때문에 시스템 라이브러리 의존을 늘릴 이유가 없다. 부수효과가 오히려
  핵심: **어떤 tty 경로든 열 수 있어 하드웨어 없이 mux 를 end-to-end 로
  테스트**할 수 있다 (테스트가 openpty 로 장치를 만든다)
- 규칙 3 중복 제거: `pty::read_fd`/`write_all_fd` 로 추출 — pty master 와
  실제 포트가 같은 구현을 쓴다. baud 테이블도 serial.rs 로 모아 rfc2217 이
  재사용. CLOCAL|CREAD, VMIN/VTIME=0, tcsetattr 후 **읽기 검증**(드라이버가
  조용히 거부한 baud 를 경고 — 조용히 틀린 속도로 도는 포트는 긴 오후다)
- `TIOCEXCL` 은 일부러 안 건다: 락·세션은 tether 의 일이고 mux 는 빠른 로컬
  분배기
- **수용 기준 통과**: `sim --preset uboot` 을 장치로 두고 mux 로 포트 2개,
  script 쪽에서 `version` → **두 소비자가 완전히 동일한 전체 스트림** 수신
  (script 가 친 입력의 에코까지). tether 가 128/72 로 쪼개졌던 바로 그
  200바이트 케이스를 통합 테스트로 고정
- 곁다리로 고친 것: **시그널 핸들러를 ready 라인 뒤에 설치하고 있었다** —
  경로를 읽자마자 SIGTERM 을 보내면 기본 동작으로 죽어 링크와 sidecar 가
  남고 종료 코드도 143. M5 teardown 테스트가 매번 재현했다. `signals::
  Shutdown` 으로 SIGTERM+SIGINT 를 **readiness 이전에** 등록하도록 4개 forge
  전부 수정 (`ctrl_c()` 는 처음 poll 될 때 등록되므로 루프 안에서는 늦다)
- 테스트 10개 (유닛 4: baud 왕복, non-tty·없는 장치 → SetupError, pty 를
  장치로 열어 양방향 / 통합 6: 200바이트 전량 팬아웃, 16KiB × 소비자 3,
  TX 병합 무결성, 한 소비자의 재open 이 다른 소비자에 무영향, SIGTERM 전체
  정리, 잘못된 장치 exit 3)

### M6 — 배포·마감 ✅ (완료)
- `--json` readiness (`src/forge/status.rs`): 기존 "한 줄에 경로 하나" 계약은
  바이트 단위로 그대로 두고, 그것으로 표현할 수 없는 것만 추가 — forge 이름,
  pid, 그리고 **와이어 시드**. 확률 기능을 쓰면 시드가 자동 생성되는데
  지금까지는 stderr 산문으로만 나갔다. 이제 하네스가 재현값을 기계적으로
  집을 수 있다. 경로는 JSON writer 로 직렬화 (`--link` 경로는 임의 텍스트고,
  손으로 이스케이프하는 순간 상태 출력이 거짓말을 시작한다)
- CI (macOS + Linux): fmt / `clippy -D warnings` / 전체 테스트 + MSRV 1.85 +
  `cargo publish --dry-run`. 포트 코어가 libc·termios 라 "컴파일된다"는
  거의 아무것도 증명하지 못하므로, 테스트가 실바이너리로 실제 pty 를 몬다
- rustfmt 도입(`use_small_heuristics = "Max"`), LICENSE-MIT/APACHE 추가,
  crates.io 메타데이터(keywords/categories/readme/exclude), README 재작성
  (설치 → readiness 프로토콜 → 레시피 5개 → 알아야 할 제약 3가지)
- v0.1.0 태그 + GitHub 릴리스. homebrew: `Formula/ttyforge.rb` (소스 빌드,
  테스트가 실제 pair 를 띄워 바이트 왕복 확인) →
  `brew audit --strict --online` 통과, `brew test` 통과, 퍼블리시된 탭에서
  `brew install hulryung/tap/ttyforge` 재설치까지 검증
- **CI 가 즉시 값을 했다** (macOS 로컬에서는 보이지 않던 것들):
  ① Linux 전용 clippy 2건 — `openpty` 의 termios 파라미터가 BSD 는 `*mut`,
    Linux 는 `*const`; `speed_t` 가 Linux 는 u32, BSD 는 u64
  ② **Linux pty 는 `tcsetattr` 을 정규화한다** (`c_cflag &= ~(CSIZE|PARENB);
    c_cflag |= CS8|CREAD`, drivers/tty/pty.c) → RFC2217 이 Linux 에서는
    데이터비트·패리티를 중계할 수 없다. 실제 러너에서 프로브로 확인하고
    문서화했으며, 테스트는 요청값이 아니라 read-back 에서 기대치를 도출하도록
    수정 (한 머신의 동작을 인코딩하지 않게)
- 곁다리로 고친 pty 코어 버그: **`ttyname_r` → `ptsname_r`**. M5 에서 "재현
  불가"로 남기고 버퍼만 키웠던 그 실패의 진짜 원인이다 — macOS 의
  `ttyname_r` 은 /dev 조회로 이름을 찾고, 그 조회 실패를 전부 ERANGE("버퍼
  부족")로 보고한다. 그래서 버퍼를 의심하게 만든다. 10개 프로세스 동시 측정:
  **4000회 중 3648회 실패**, 버퍼 32B~4KiB 전부 동일. `ptsname_r` 은 같은
  pty 에서 4000/4000 성공하고 이름도 일치한다 — 커널에 직접 묻기 때문
  (TIOCPTYGNAME). 회귀 테스트는 4스레드 동시 생성으로 **이전 코드에서 5/5
  실패, 수정 후 5/5 통과**. mux 를 pair 옆에서 돌리기만 해도 물리던 버그
- **crates.io publish 완료**: v0.1.0. 레지스트리에서 되받아
  `cargo install ttyforge --locked` 로 재설치해 U-Boot sim ↔ pyserial 대화까지
  확인
- `scripts/acceptance/` 로 마일스톤 수용 검증을 저장소에 편입 (M4a socat,
  M4b pyserial RFC2217 서버, M5 소비자 2개) + `workflow_dispatch` 워크플로로
  양 OS 실행. 스크래치에 있던 원본을 그대로 옮기지 않고 고쳤다: **불일치에도
  exit 0 으로 끝나고 있었고**(실패를 알릴 수 없는 스크립트는 없느니만 못하다),
  `sleep` 으로 준비를 기다리고 있었으며(이제 readiness 라인을 읽는다 — 자기가
  검증하는 계약을 스스로 사용), 절대경로·고정 포트가 박혀 있었다. 통과/불일치/
  바이너리 없음/도구 없음 네 경로를 모두 실측
- 워크플로 작성 중 **Linux 전용 버그를 하나 더 잡았다**: 포트 경로를
  `mktemp -u /tmp/…XXXXXX.pty` 로 만들고 있었는데 GNU mktemp 는 X 가 끝에
  없는 템플릿을 거부한다 — macOS 는 통과, Linux 만 실패하는 가장 나쁜 종류

## 5. 플랫폼·제약 (미리 못박는 것)

- **unix 전용** (macOS + Linux). Windows 가상 COM 은 com0com/커널 드라이버
  영역이라 범위 밖 — README 에 명시.
- pty 에는 UART 가 없다: 소비자가 가상 포트에 건 baud 변경은 no-op
  (M3 `--baud-sim` 이 시뮬레이션 담당), DTR/RTS 는 전달 불가.
  tether 문서와 동일한 제약 — 동일 문구로 문서화.
- 에디션 2021 (tether 코드 이식 마찰 최소화), rust 1.85+ (CI 가 강제).
- **pty 는 CSIZE 를 데이터에 적용하지 않는다** (양 OS 실측): CS7 인 슬레이브도
  0xFF 를 그대로 통과시킨다. 그래서 RFC2217 모드에서 c_cflag 를 보존해도
  바이너리 투명성이 깨지지 않는다.
- **Linux pty 는 tcsetattr 을 정규화한다** (`c_cflag &= ~(CSIZE|PARENB);
  c_cflag |= CS8|CREAD`): 데이터비트·패리티는 애초에 관측 자체가 불가능하다.
  RFC2217 도달 범위가 OS 마다 다른 이유 (§4 M4b).
- **pty 슬레이브 이름은 `ptsname_r` 로 얻는다**: macOS 의 `ttyname_r` 은
  /dev 조회 실패를 전부 ERANGE 로 보고해 동시 생성에서 무너진다.
  macOS 10.13.4+ 필요 (libc 크레이트가 Linux 에만 선언해 직접 extern 선언).
- 시스템의 pty 총량에 상한이 있다 (macOS `kern.tty.ptmx_max`, 기본 511).
  `openpty` 는 실패해도 errno 를 신뢰할 수 없게 두므로 그 사실을 메시지로
  말해준다.

## 6. 리스크 / 열어둔 결정

- ~~**RFC2217 범위**: termios 폴링 vs TIOCPKT 이벤트~~ → M4b 에서 폴링(A)
  으로 결정·구현 완료. TIOCPKT 는 ^S/^Q 나 PURGE-DATA 가 실제로 필요해질
  때, **RFC2217 모드에 한정해서만** 얹는다 (pair/sim 의 read 경로는 불변).
- **tether TCP 상호운용**: `tetherd --tcp` 는 raw 시리얼이 아니라 NDJSON/
  JSON-RPC 2.0 + 토큰 인증이다. 붙이려면 `bridge` 가 아니라 별도의 "tether
  클라이언트 모드"(hello/attach/send + data 알림 언랩)가 필요하다 — 할지
  말지 열린 결정. 안 하면 `tether -D <dev>,pty` 가 이미 그 역할을 한다.
- **반쪽 죽은 피어**: TCP keepalive 미설정(socket2 의존성 회피). 네트워크가
  조용히 끊기면 세션이 FIN 없이 매달린다 — 실제로 물리면 keepalive 또는
  유휴 타임아웃 추가.
- **preset 확장**: rhai/lua 스크립팅 요구가 생기면 M2 exec 백엔드로
  충분한지 재평가 (일단 "장치는 그냥 프로그램" 철학 유지).
- **crate 분리**: `ttyforge-pty` 라이브러리로 분리해 serial-tether 가 역으로
  의존하는 옵션 (중복 제거). 지금은 판단 근거가 하나 늘었다 — `ptsname_r`
  건은 tether 의 pty 코어에도 그대로 있을 가능성이 높고, 공유 crate 였다면
  한 번만 고치면 됐다.

## 7. 다음에 할 만한 것 (v0.1.0 이후)

마일스톤은 다 끝났다. 아래는 "필요해지면" 목록이지 계획이 아니다 — 실제
사용에서 아프지 않은 것을 미리 만들지 않는 게 여기까지의 방침이었다.

- **tether 로 발견한 버그 역이식**: `ptsname_r` 건은 serial-tether 의 pty
  코어에도 있을 공산이 크다. 확인해서 고치는 것이 가장 값싼 다음 작업.
- **`--rfc2217` 서버 모드**: 지금은 클라이언트뿐이다. ttyforge 가 ser2net
  자리에 서는 형태(가상 포트를 RFC2217 로 내보내기)는 자연스러운 대칭이지만,
  요구가 실제로 생기기 전에는 만들 이유가 없다.
- **`mux` 의 TX 정책**: 지금은 청크 단위 병합. 쓰기 락이나 "한 소비자만 rw"
  같은 정책이 필요해지면 그건 tether 영역과 겹치기 시작하는 신호다.
- **바이너리 릴리스**: homebrew 는 소스 빌드라 rust 툴체인이 필요하다. 사전
  빌드 아티팩트가 필요해지면 릴리스 워크플로 추가.
- **회귀 감시**: 릴리스 전에 `gh workflow run acceptance.yml` — 상대편
  구현(ser2net·pyserial)이 바뀌었을 때 알아차리는 유일한 장치다.
