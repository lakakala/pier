"""Exercise terminal output draining when deployment has closed the transport."""
import io
import json
import unittest
from unittest.mock import Mock

from terminal_package_checks import TerminalSocket


def frame(opcode, data):
    assert len(data) < 126
    return bytes([0x80 | opcode, len(data)]) + data


def terminal(data, closed=False):
    ws = TerminalSocket.__new__(TerminalSocket)
    ws.socket = Mock()
    ws.socket.recv.side_effect = io.BytesIO(data).read
    if closed:
        ws.socket.sendall.side_effect = BrokenPipeError('peer already closed')
    return ws


def sent_payload(call):
    data = call.args[0]
    mask = data[2:6]
    return bytes(byte ^ mask[i % 4] for i, byte in enumerate(data[6:]))


class TerminalSocketTests(unittest.TestCase):
    def test_open_terminal_acknowledges_output_and_answers_ping(self):
        ws = terminal(frame(9, b'ping') + frame(2, b'output'))
        self.assertEqual(ws.receive(), b'output')
        pong, ack = ws.socket.sendall.call_args_list
        self.assertEqual(pong.args[0][0], 0x8a)
        self.assertEqual(sent_payload(pong), b'ping')
        self.assertEqual(json.loads(sent_payload(ack)), {'type': 'ack', 'bytes': 6})

    def test_deployment_exit_is_read_after_buffered_output_on_closed_transport(self):
        event = {'type': 'exit', 'reason': 'deployment_started'}
        ws = terminal(frame(9, b'ping') + frame(2, b'last output') +
                      frame(1, json.dumps(event).encode()), closed=True)
        self.assertEqual(ws.receive(closing=True), b'last output')
        self.assertEqual(ws.receive(closing=True), event)
        ws.socket.sendall.assert_not_called()

    def test_closed_transport_without_exit_still_fails(self):
        for tail in (b'', frame(8, b'')):
            with self.subTest(tail=tail):
                ws = terminal(frame(2, b'last output') + tail, closed=True)
                self.assertEqual(ws.receive(closing=True), b'last output')
                with self.assertRaises(EOFError):
                    ws.receive(closing=True)


if __name__ == '__main__':
    unittest.main()
