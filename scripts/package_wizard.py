"""PTY driver shared by native package tests; never prints credentials."""
import os
import pty
import select
import termios
import time


class Wizard:
    def __init__(self, term='dumb', prefix=None):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ['TERM'] = term
            argv = (prefix or []) + ['/usr/bin/pier-agent', 'init']
            os.execvp(argv[0], argv)
        self.buffer = b''
        self.cursor = 0
        self.original_terminal = termios.tcgetattr(self.fd)

    def expect(self, marker, timeout=60):
        needle = marker.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            index = self.buffer.find(needle, self.cursor)
            if index >= 0:
                self.cursor = index + len(needle)
                return
            if select.select([self.fd], [], [], 0.2)[0]:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.buffer += data
        # Do not dump the buffer: an echo regression could contain credentials.
        raise AssertionError('wizard did not show expected prompt: ' + marker)

    def send(self, value):
        os.write(self.fd, value.encode())

    def finish(self, success=True):
        def exited():
            result = os.waitpid(self.pid, os.WNOHANG)
            return result if result[0] else None
        deadline = time.monotonic() + 30
        result = None
        while time.monotonic() < deadline:
            result = exited()
            if result:
                break
            time.sleep(0.1)
        assert result, 'wizard did not exit'
        if success:
            assert os.WIFEXITED(result[1]) and os.WEXITSTATUS(result[1]) == 0, 'wizard failed'
        now = termios.tcgetattr(self.fd)
        assert now[3] & termios.ECHO, 'terminal echo was not restored'
        os.close(self.fd)

