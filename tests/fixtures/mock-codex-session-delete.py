#!/usr/bin/env python3
"""Shared WebSocket service and raw Codex proxy, entirely inside a test home."""
import base64
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import sqlite3
import struct
import sys
import threading

LIMIT = 8 * 1024 * 1024


class Connection:
    def __init__(self, stream):
        self.stream = stream
        self.lock = threading.Lock()
        self.initialized = False

    def read_exact(self, count):
        result = bytearray()
        while len(result) < count:
            block = self.stream.recv(count - len(result))
            if not block:
                raise EOFError()
            result.extend(block)
        return bytes(result)

    def receive(self):
        while True:
            first, second = self.read_exact(2)
            count = second & 127
            if count == 126:
                count = struct.unpack('!H', self.read_exact(2))[0]
            elif count == 127:
                count = struct.unpack('!Q', self.read_exact(8))[0]
            assert count <= LIMIT and first & 128, "bounded complete frames required"
            mask = self.read_exact(4) if second & 128 else None
            data = self.read_exact(count)
            if mask:
                data = bytes(value ^ mask[index % 4] for index, value in enumerate(data))
            opcode = first & 15
            if opcode == 8:
                raise EOFError()
            if opcode == 9:
                self.send_frame(10, data)
                continue
            assert opcode == 1, "JSON text frames only"
            return json.loads(data)

    def send_frame(self, opcode, data):
        count = len(data)
        header = bytes([128 | opcode, count]) if count < 126 else (
            bytes([128 | opcode, 126]) + struct.pack('!H', count) if count < 65536 else
            bytes([128 | opcode, 127]) + struct.pack('!Q', count))
        with self.lock:
            self.stream.sendall(header + data)

    def send(self, message):
        self.send_frame(1, json.dumps(message).encode())


class Service:
    def __init__(self, home):
        self.home = home
        self.connections = set()
        self.lock = threading.RLock()

    def broadcast(self, message):
        with self.lock:
            peers = [peer for peer in self.connections if peer.initialized]
        for peer in peers:
            try:
                peer.send(message)
            except OSError:
                pass

    def handle(self, stream):
        peer = Connection(stream)
        try:
            headers = bytearray()
            while not headers.endswith(b'\r\n\r\n'):
                headers.extend(peer.read_exact(1))
                assert len(headers) < 65536, "bounded handshake required"
            fields = {}
            for line in headers.decode().split('\r\n')[1:]:
                if ':' in line:
                    key, value = line.split(':', 1)
                    fields[key.lower()] = value.strip()
            accept = base64.b64encode(hashlib.sha1((fields['sec-websocket-key'] +
                '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
            stream.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n'
                'Connection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n').encode())
            with self.lock:
                self.connections.add(peer)
            while True:
                request = peer.receive()
                with self.lock:
                    with (self.home / 'management-requests.jsonl').open('a') as log:
                        log.write(json.dumps(request) + '\n')
                method = request['method']
                if 'id' not in request:
                    assert method == 'initialized'
                    peer.initialized = True
                    continue
                try:
                    manifest = json.loads((self.home / 'management-fixture.json').read_text())
                    result, deleted = self.dispatch(method, request.get('params', {}), manifest)
                    notifications = [value for value in deleted
                        if value not in manifest.get('missing_notifications', [])]
                    if deleted:
                        notifications.extend(manifest.get('unrelated_notifications', []))
                    if manifest.get('notifications_before_response'):
                        for value in notifications:
                            self.broadcast({'method': 'thread/deleted', 'params': {'threadId': value}})
                    peer.send({'id': request['id'], 'result': result})
                    if not manifest.get('notifications_before_response'):
                        for value in notifications:
                            self.broadcast({'method': 'thread/deleted', 'params': {'threadId': value}})
                except MissingThread as error:
                    peer.send({'id': request['id'], 'error': {
                        'code': -32600, 'message': str(error)}})
                except Exception as error:
                    peer.send({'id': request['id'], 'error': {
                        'code': -32602, 'message': 'fixture rejected request: ' + str(error)}})
        except (EOFError, ConnectionError, OSError):
            pass
        finally:
            with self.lock:
                self.connections.discard(peer)
            stream.close()

    def dispatch(self, method, params, manifest):
        threads = manifest['threads']
        if method == 'initialize':
            assert params['capabilities']['experimentalApi'] is True
            return {'userAgent': 'shared-session-delete-fixture'}, []
        if method == 'thread/read':
            target = threads[params['threadId']]
            if not Path(target['path']).exists():
                prefix = 'thread not loaded: ' if manifest.get('missing_not_loaded') else 'no rollout found for thread id '
                # Match the real server's definite missing-state diagnostic.
                raise MissingThread(prefix + target['id'])
            if manifest.get('mismatched_identity'):
                target = dict(target, id='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa')
            return {'thread': target}, []
        if method == 'thread/list':
            ancestor = params.get('ancestorThreadId')
            assert ancestor in (None, manifest['root'])
            assert params['modelProviders'] == []
            assert set(params['sourceKinds']) == {'cli', 'vscode', 'exec', 'appServer', 'subAgent',
                'subAgentReview', 'subAgentCompact', 'subAgentThreadSpawn', 'subAgentOther', 'unknown'}
            assert params['useStateDbOnly'] is True
            matches = [thread for thread in threads.values()
                if (ancestor is None or thread.get('parentThreadId') == ancestor)
                and thread['archived'] == params['archived'] and Path(thread['path']).exists()]
            if ancestor is None:
                matches.extend(thread for thread in manifest.get('inventory_extra_threads', [])
                    if thread['archived'] == params['archived'])
            matches.sort(key=lambda thread: thread['id'])
            offset = int(params.get('cursor') or '0')
            result = {'data': matches[offset:offset + 1],
                'nextCursor': str(offset + 1) if offset + 1 < len(matches) else None}
            if ancestor is None and manifest.get('inventory_repeating_cursor'):
                result['nextCursor'] = '1'
            return result, []
        if method == 'thread/delete':
            assert params == {'threadId': manifest['root']}
            # A separate, already-running server must observe a committed ledger.
            with sqlite3.connect(Path(manifest['ledger']).as_uri() + '?mode=ro', uri=True) as db:
                records = [json.loads(row[0]) for row in db.execute('SELECT payload FROM sessions')]
            assert {record['threadId'] for record in records} == set(threads)
            assert all(record['status'] == 'pending' for record in records)
            assert sum(call['tokens']['totalTokens'] for record in records
                for call in record['calls']) == manifest['tokens']
            roots = [(self.home / name).resolve(strict=True) for name in ('sessions', 'archived_sessions')]
            assert all(root.is_relative_to(self.home.parent) for root in roots), 'fixture roots escaped temp'
            for thread in threads.values():
                source = Path(thread['path']).resolve()
                assert any(source.is_relative_to(root) for root in roots), 'fixture log escaped its roots'
                source.rename(self.home / ('retired-' + thread['id']))
            result = {'unrecognized': True} if manifest.get('invalid_delete_reply') else {}
            return result, list(threads)
        raise AssertionError('unapproved method ' + method)


class MissingThread(Exception):
    pass


def serve(home, physical):
    configured = home / 'app-server-control' / 'app-server-control.sock'
    configured.parent.mkdir(mode=0o700, exist_ok=True)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(physical))
    os.chmod(physical, 0o600)
    configured.unlink(missing_ok=True)
    configured.symlink_to(physical)
    listener.listen(16)
    service = Service(home)
    print(json.dumps({'ready': True, 'socket': str(configured)}), flush=True)
    while True:
        stream, _ = listener.accept()
        threading.Thread(target=service.handle, args=(stream,), daemon=True).start()


def proxy(home, arguments):
    with (home / 'proxy-launches.jsonl').open('a') as log:
        log.write(json.dumps(arguments) + '\n')
    assert len(arguments) == 4 and arguments[:3] == ['app-server', 'proxy', '--sock'], \
        'only the existing-server proxy may be started'
    path = Path(arguments[3]).resolve(strict=True)
    assert path.is_relative_to(home.parent), 'proxy socket escaped fixture temp'
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.connect(str(path))
    while True:
        ready, _, _ = select.select([stream, sys.stdin.buffer], [], [])
        if stream in ready:
            block = stream.recv(65536)
            if not block:
                return
            sys.stdout.buffer.write(block)
            sys.stdout.buffer.flush()
        if sys.stdin.buffer in ready:
            block = os.read(sys.stdin.fileno(), 65536)
            if not block:
                return
            stream.sendall(block)


def main():
    home = Path(os.environ['CODEX_HOME']).resolve(strict=True)
    arguments = sys.argv[1:]
    if len(arguments) == 2 and arguments[0] == '--serve':
        serve(home, Path(arguments[1]))
    else:
        proxy(home, arguments)


if __name__ == '__main__':
    main()
