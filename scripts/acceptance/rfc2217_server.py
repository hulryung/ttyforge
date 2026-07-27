"""A third-party RFC2217 server: pyserial's own PortManager in front of a
loopback pty, so the acceptance script can watch which line settings actually
get applied to the far-end "hardware".

Nothing here is ttyforge code — that is the entire point of using it."""

import os
import socket
import sys
import threading
import time

import serial
import serial.rfc2217


class PtySerial(serial.Serial):
    """A pty has no modem lines, so TIOCMGET raises ENOTTY and pyserial's
    modem-state notifications would kill the server on the first client.
    Stub them: RFC2217 modem state is out of ttyforge's scope either way (a
    pty gives the forge nothing to observe), while the *line settings* this
    script checks are real. Found the hard way — the traceback from the
    unstubbed version is what confirmed the DTR/RTS limitation."""

    cts = property(lambda self: False)
    dsr = property(lambda self: False)
    ri = property(lambda self: False)
    cd = property(lambda self: False)


def main(port: int) -> None:
    master, slave = os.openpty()
    ser = PtySerial(os.ttyname(slave), timeout=0.05)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(1)
    print("READY", flush=True)
    sock, _ = srv.accept()

    class Conn:
        def write(self, data):
            sock.sendall(data)

    mgr = serial.rfc2217.PortManager(ser, Conn())

    def watch():  # report every settings change the client asks for
        last = None
        while True:
            cur = (ser.baudrate, ser.bytesize, ser.parity, ser.stopbits, ser.rtscts)
            if cur != last:
                print("SETTINGS %r" % (cur,), flush=True)
                last = cur
            time.sleep(0.03)

    def loopback():  # the "device" behind the server's port, echoing
        while True:
            try:
                os.write(master, os.read(master, 4096))
            except OSError:
                return

    def to_client():
        while True:
            data = ser.read(1024)
            if data:
                sock.sendall(b"".join(mgr.escape(data)))

    for fn in (watch, loopback, to_client):
        threading.Thread(target=fn, daemon=True).start()

    while True:
        data = sock.recv(4096)
        if not data:
            break
        ser.write(b"".join(mgr.filter(data)))


if __name__ == "__main__":
    main(int(sys.argv[1]))
