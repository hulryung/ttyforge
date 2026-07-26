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

### M4 — `bridge` ✅ (M4a raw TCP 완료 / M4b RFC2217 미착수)
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
- 테스트 13개 (유닛 10: 이스케이프 왕복·모든 경계에서의 분할 디코딩·미지
  옵션 거부와 ack 루프 방지·subneg 파라미터 이스케이프·termios 매핑·변경분만
  전송·발신 명령 자기 디코딩 + rule 5 분리 / 통합 3: 설정 릴레이와 유휴 시
  침묵과 변경분만, 바이너리 투명성, listen 모드 거부 exit 3)

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

- ~~**RFC2217 범위**: termios 폴링 vs TIOCPKT 이벤트~~ → M4b 에서 폴링(A)
  으로 결정·구현 완료. TIOCPKT 는 ^S/^Q 나 PURGE-DATA 가 실제로 필요해질
  때, **RFC2217 모드에 한정해서만** 얹는다 (pair/sim 의 read 경로는 불변).
- **tether TCP 상호운용**: `tetherd --tcp` 는 raw 시리얼이 아니라 NDJSON/
  JSON-RPC 2.0 + 토큰 인증이다. 붙이려면 `bridge` 가 아니라 별도의 "tether
  클라이언트 모드"(hello/attach/send + data 알림 언랩)가 필요하다 — 할지
  말지 열린 결정. 안 하면 `tether -D <dev>,pty` 가 이미 그 역할을 한다.
- **반쪽 죽은 피어**: TCP keepalive 미설정(socket2 의존성 회피). 네트워크가
  조용히 끊기면 세션이 FIN 없이 매달린다 — 실제 문제가 되면 M4b 에서
  keepalive 또는 유휴 타임아웃 추가.
- **preset 확장**: rhai/lua 스크립팅 요구가 생기면 M2 exec 백엔드로
  충분한지 재평가 (일단 "장치는 그냥 프로그램" 철학 유지).
- **crate 분리**: pty 코어가 안정되면 `ttyforge-pty` 라이브러리로 분리해
  serial-tether 가 역으로 의존하는 옵션 (중복 제거). M6 이후 판단.
