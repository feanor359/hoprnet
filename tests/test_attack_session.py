import asyncio
import http.server
import logging
import multiprocessing
import os
import csv
import random
import re
import socket
import ssl
import string
import subprocess
import threading
import time
from contextlib import AsyncExitStack, contextmanager
from datetime import datetime, timedelta
from enum import Enum
from functools import partial

import pytest
import requests
import urllib3
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

from sdk.python.api.protocol import Protocol
from sdk.python.api.request_objects import SessionCapabilitiesBody
from sdk.python.api.response_objects import Session
from sdk.python.localcluster.constants import MAIN_DIR, TICKET_PRICE_PER_HOP, ANVIL_CONFIG_FILE, NETWORK, CONTRACTS_DIR, PORT_BASE
from sdk.python.localcluster.node import Node
from tests.config_nodes import test_session_distruption

from .conftest import  session_attack_nodes, run_hopli_cmd
from .utils import create_channel

HOPR_SESSION_MAX_PAYLOAD_SIZE = 462
STANDARD_MTU_SIZE = 1500

ANVIL_ENDPOINT = f"http://127.0.0.1:{PORT_BASE}"

def set_minimum_winning_probability_in_network(private_key: str, win_prob: float):
    custom_env = {"PRIVATE_KEY": private_key}
    cmd = [
        "hopli",
        "win-prob",
        "set",
        "--network",
        NETWORK,
        "--contracts-root",
        CONTRACTS_DIR,
        "--winning-probability",
        str(win_prob),
        "--provider-url",
        ANVIL_ENDPOINT,
    ]
    run_hopli_cmd(cmd, custom_env)


class SocketType(Enum):
    TCP = 1
    UDP = 2


class EchoServer:
    def __init__(self, server_type: SocketType, recv_buf_len: int, with_tcpdump: bool = False):
        self.server_type = server_type
        self.port = None
        self.process = None
        self.with_tcpdump = with_tcpdump
        self.tcp_dump_process = None
        self.socket = None
        self.recv_buf_len = recv_buf_len

    def __enter__(self):
        if self.server_type is SocketType.TCP:
            self.socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        else:
            self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

        self.socket.bind(("127.0.0.1", 0))
        self.port = self.socket.getsockname()[1]
        logging.info(f"echo server listening on port {self.port}")

        if self.server_type is SocketType.TCP:
            self.socket.listen()
            self.process = multiprocessing.Process(target=tcp_echo_server_func, args=(self.socket, self.recv_buf_len))
        else:
            self.process = multiprocessing.Process(target=udp_echo_server_func, args=(self.socket, self.recv_buf_len))
        self.process.start()

        # If needed, tcp dump can be started to catch traffic on the local interface
        if self.with_tcpdump:
            test_session_dir = MAIN_DIR.joinpath("test_session")
            if not test_session_dir.exists():
                test_session_dir.mkdir(parents=True, exist_ok=True)
            logging.info(f"Created directory: {test_session_dir}")

            pcap_file = test_session_dir.joinpath(f"echo_server_{self.port}.pcap")
            self.tcp_dump_process = subprocess.Popen(
                ["sudo", "tcpdump", "-i", "lo", "-w", f"{pcap_file}.log"],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            logging.info(f"running tcpdump, saving to {pcap_file}.log")

        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.process.terminate()
        self.socket.close()
        self.socket = None
        self.process = None
        self.port = None

        if self.with_tcpdump:
            logging.info("killing tcp dump")
            self.tcp_dump_process.kill() # Kill the process first
            stdout, stderr = self.tcp_dump_process.communicate() # Then communicate
            self.tcp_dump_process = None
            logging.info(f"terminated tcpdump: {stdout}, {stderr}")
        return True


def tcp_echo_server_func(s, buf_len):
    conn, _addr = s.accept()
    with conn:
        while True:
            data = conn.recv(buf_len)
            conn.sendall(data)
            log_data = re.sub(r"\s+", "", data.decode())
            # logging.info(f"Data are: {log_data}")


def udp_echo_server_func(s, buf_len):
    while True:
        data, addr = s.recvfrom(buf_len)
        s.sendto(data, addr)


@contextmanager
def connect_socket(sock_type: SocketType, port):
    if (sock_type is SocketType.TCP):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.connect(("127.0.0.1", port))
    elif sock_type is SocketType.UDP:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    else:
        raise ValueError("Invalid socket type")

    try:
        yield s
    finally:
        s.close()


def fetch_data(url: str):
    # Suppress only the single InsecureRequestWarning from urllib3 needed for self-signed certs
    urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

    try:
        # set verify=False for self-signed certs
        response = requests.get(url, verify=False)
        response.raise_for_status()
        return response
    except requests.exceptions.RequestException as e:
        logging.error(f"HTTP request failed: {e}")
        return None


def generate_self_signed_cert(cert_file_with_key):
    key = rsa.generate_private_key(
        public_exponent=65537,
        key_size=2048,
    )

    subject = issuer = x509.Name(
        [
            x509.NameAttribute(NameOID.COUNTRY_NAME, "CH"),
            x509.NameAttribute(NameOID.STATE_OR_PROVINCE_NAME, "Switzerland"),
            x509.NameAttribute(NameOID.LOCALITY_NAME, "Zurich"),
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "HOPR"),
            x509.NameAttribute(NameOID.COMMON_NAME, "localhost"),
        ]
    )

    cert = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.utcnow())
        .not_valid_after(datetime.utcnow() + timedelta(days=365))
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName("localhost")]),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )

    # Combine the private key and certificate into a single PEM file
    with open(cert_file_with_key, "wb") as f:
        f.write(
            key.private_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PrivateFormat.TraditionalOpenSSL,
                encryption_algorithm=serialization.NoEncryption(),
            )
        )
        f.write(cert.public_bytes(serialization.Encoding.PEM))


class CustomHTTPRequestHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, content=None, *args, **kwargs):
        self.content = content
        super().__init__(*args, **kwargs)

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-type", "text/plain")
        self.end_headers()
        self.wfile.write(self.content.encode("utf-8"))
        time.sleep(1)  # add an artificial delay


@contextmanager
def run_https_server(served_text_content):
    cert_file = "cert.pem"

    # Generate the certificate and key if they don't exist
    if not os.path.exists(cert_file):
        logging.debug("generating self-signed certificate and key...")
        generate_self_signed_cert(cert_file)

    # Create a handler class with the content passed
    handler_class = partial(CustomHTTPRequestHandler, served_text_content)

    # Set up the HTTP server with a random port and SSL context
    httpd = http.server.HTTPServer(("localhost", 0), handler_class)
    ssl_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ssl_context.load_cert_chain(certfile=cert_file, keyfile=cert_file)
    httpd.socket = ssl_context.wrap_socket(httpd.socket, server_side=True)

    # Get the random port assigned to the server
    port = httpd.server_address[1]
    server_thread = threading.Thread(target=httpd.serve_forever)
    try:
        server_thread.start()
        logging.debug(f"serving on https://localhost:{port}")
        yield port
    finally:
        logging.debug("shutting down the HTTP server...")
        httpd.shutdown()
        httpd.server_close()
        server_thread.join()
        if os.path.exists(cert_file):
            os.remove(cert_file)


class TestSessionWithSwarm:
    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        "route",
        [session_attack_nodes()]
    )
    @pytest.mark.parametrize(
        "nodes_config",
        [
            {
                "local1": {
                    "HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS": 1,
                    "HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS": 1
                },
                "local2": setting,
                "local3": {
                    "HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS": 1,
                    "HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS": 1,
                    "HOPR_DISABLE_ACK_PACKAGES": True
                }
            } for setting in test_session_distruption() # since the setting is too long and diverse, load it from external file
        ]
    )
    async def test_session_communication_with_a_tcp_echo_server_with_random_delay(self, route, swarm3: dict[str, Node], nodes_config: dict[str, dict], config_to_yaml: str):
        """
        Test forming tcp session over the hopr mixnet while relay node introduces random delay
        """
          
        packet_count = 100 if os.getenv("CI", default="false") == "false" else 50
        expected = [f"{i}".ljust(STANDARD_MTU_SIZE) for i in range(packet_count)]

        assert [len(x) for x in expected] == packet_count * [STANDARD_MTU_SIZE]

        src_peer = swarm3[route[0]]
        dest_peer = swarm3[route[-1]]
        path = [swarm3[node].peer_id for node in route[1:-1]]

        # #print winning probability of the node
        # win_prob_object = await swarm3[route[1]].api.ticket_min_win_prob()
        # logging.info(f"Current minimum winning probability: {win_prob_object.value}")
        # #print the first node on the path
        # logging.info(f"src_peer: {src_peer.alias}, dest_peer: {dest_peer.alias}")
        # logging.info(f"src_peer (local1) API port: {src_peer.api_port}, P2P port: {src_peer.p2p_port}")
        # logging.info(f"dest_peer (local3) API port: {dest_peer.api_port}, P2P port: {dest_peer.p2p_port}")

        async with AsyncExitStack() as channels:
            channels_to = [
                channels.enter_async_context(
                    create_channel(
                        swarm3[route[i]], swarm3[route[i + 1]], funding=20 * packet_count * TICKET_PRICE_PER_HOP
                    )
                )
                for i in range(len(route) - 1)
            ]
            channels_back = [
                channels.enter_async_context(
                    create_channel(
                        swarm3[route[i]], swarm3[route[i - 1]], funding=20 * packet_count * TICKET_PRICE_PER_HOP
                    )
                )
                for i in reversed(range(1, len(route)))
            ]

            await asyncio.gather(*(channels_to + channels_back))

            actual = ""
            with EchoServer(SocketType.TCP, STANDARD_MTU_SIZE) as server:
                await asyncio.sleep(1.0)

                session = await src_peer.api.session_client(
                    dest_peer.peer_id,
                    path={"IntermediatePath": path},
                    protocol=Protocol.TCP,
                    target=f"localhost:{server.port}",
                    capabilities=SessionCapabilitiesBody(retransmission=True, segmentation=True),
                )

                assert session.port is not None, "Failed to open session"
                assert len(await src_peer.api.session_list_clients(Protocol.TCP)) == 1

                with connect_socket(SocketType.TCP, session.port) as s:
                    s.settimeout(3600)
                    total_sent = 0
                    for message in expected:
                        total_sent = total_sent + s.send(message.encode())

                    while total_sent > 0:
                        chunk = s.recv(min(STANDARD_MTU_SIZE, total_sent))
                        total_sent = total_sent - len(chunk)
                        actual = actual + chunk.decode()

            assert await src_peer.api.session_close_client(session) is True
            assert await src_peer.api.session_list_clients(Protocol.TCP) == []

            # log into csv file
            ###################
            csv_file_path = os.path.join("logs/session_attack", f"session_attack.csv")
            local2_conf = nodes_config.get("local2", {})   
            min_delay = local2_conf.get("HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS", "N/A")
            delay_range = local2_conf.get("HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS", "N/A")
        
            metrics = await swarm3[route[1]].api.metrics()
            metrics = "\n".join(line for line in metrics.splitlines() if not line.startswith("#"))
            metrics_dict = dict(line.split(maxsplit=1) for line in metrics.splitlines())
            # logging.info(f"Current node local2 metrics: {metrics}")

            avg_packet_delay = float(metrics_dict.get("hopr_mixer_average_packet_delay", 0))
            packets_relayed = float(metrics_dict.get("hopr_packets_count{type=\"forwarded\"}", 0))
            # ack_count_local2 = float(metrics_dict.get("hopr_received_ack_count", 0))
        
            # logging.info(f"ack_count_local2: {ack_count_local2}")
            logging.info(f"relay node average packet delay: {avg_packet_delay}");
            logging.info(f"relay node packets relayed: {packets_relayed}");

            timestamp = datetime.now().isoformat()
            data = [timestamp, min_delay, delay_range, avg_packet_delay, packets_relayed]
            csv_header = ["timestamp", "relay_min_delay_ms", "relay_delay_range_ms", "num_sent_messages", "avg_packet_delay_ms", "packets_relayed"]

            file_is_empty_or_does_not_exist = not (os.path.exists(csv_file_path) and os.path.getsize(csv_file_path) > 0)
                
            with open(csv_file_path, "a", newline="") as f_csv:
                    csv_writer = csv.writer(f_csv)
                    if file_is_empty_or_does_not_exist:
                        csv_writer.writerow(csv_header)
                    csv_writer.writerow(data)
            ###################

            # min_delay 10000 is expected to fail
            if min_delay == 10000:
                pytest.xfail("Test is expected to fail due to long min_delay")   

            # assert correct data at the end - long delays can break the tcp sesstion
            # and cause data loss - this is valuable information too
            assert "".join(expected) == actual