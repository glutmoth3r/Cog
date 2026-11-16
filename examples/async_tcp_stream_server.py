#!/usr/bin/env python3
"""Loopback peer for async_tcp_stream.cog."""

import socket


with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 18766))
    listener.listen(1)
    print("Listening on 127.0.0.1:18766; start examples/async_tcp_stream.cog", flush=True)
    connection, _ = listener.accept()
    with connection:
        connection.sendall(b"ready")
        request = connection.recv(64)
        connection.sendall(b"ack:" + request)
        connection.shutdown(socket.SHUT_WR)
        while connection.recv(4096):
            pass
