import asyncio
import logging
import time
import matplotlib.pyplot as plt
import numpy as np
from datetime import datetime
import os

import pytest

from sdk.python.localcluster.constants import (
    TICKET_PRICE_PER_HOP,
)
from sdk.python.localcluster.node import Node

from .conftest import attacking_nodes, first_two_from
from .utils import (
    create_channel,
    check_received_packets_with_pop,
    send_and_receive_packets_with_pop,
    gen_random_tag,
)
from .plotting_utils import plot_transfer_time_histogram

PORT_BASE = 19000

DEFAULT_ARGS = [
    ("hops", 0),
    ("capabilities", "Segmentation"),
    ("capabilities", "Retransmission"),
    ("target", "127.0.0.1:4677"),
]

class TestAttacksWithSwarm:
    @pytest.mark.asyncio
    @pytest.mark.parametrize("src, dest", first_two_from(attacking_nodes()))
    @pytest.mark.parametrize(
        "nodes_config", 
        [
            (
                {
                "local1": {
                    "HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS": 5000,
                    "HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS": 1
                },
                "local2": {
                }
                }
            ),
        ],                 
    )
    async def test_delay_msg(self, src: Node, dest: Node, swarm3: dict[str, Node], nodes_config: dict[str, dict], config_to_yaml: str):
        """
        Test delaying messages
        """

        message_count = 1
        packets = [f"0 hop message with delay #{i:08d}" for i in range(message_count)]
        logging.info(f"Sending {message_count} packets from {src} to {dest}")

        start_time = time.monotonic()

        await send_and_receive_packets_with_pop(packets, src=swarm3[src], dest=swarm3[dest], path=[], timeout=10000)

        end_time = time.monotonic()

        duration = end_time - start_time  # Calculate the duration
        logging.info(f"Async operation took {duration:.4f} seconds")

        if src == "local1":
            assert duration < 5, "The message was not delayed as expected"
            assert duration > 6, "The message was delayed too long"

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        "nodes_config", 
        [
            {
                f"local{i}": {
                    "HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS": 1000 * j if i == 1 else 1,
                    "HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS": 1000 if i == 1 else 1,
                } if i in [1, 2] else {}
                for i in range(1, 4)
            }
            for j in range(1, 4)
        ]
    )
    @pytest.mark.parametrize("src, dest", first_two_from(attacking_nodes()))
    async def test_delay_messages_gives_uniform_distribution(
        self, src: Node, dest: Node, swarm3: dict[str, Node], 
        nodes_config: dict[str, dict], config_to_yaml: str
    ):
        """
        Test if delaying messages on one hop route gives a uniform distribution.
        Measures time from sending until message is confirmed received via API polling.
        """
        os.makedirs("logs", exist_ok=True)

        # create unqiue timestamp that will be used in name of file later
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")

        # number of sent messages
        message_count = 1000
        packets = [
            {"index": i, "content": f"Message #{i:08d}"}
            for i in range(message_count)
        ]

        logging.info(f"Sending {message_count} packets from {src} to {dest}")

        packet_times = {}

        channel_funding = message_count * TICKET_PRICE_PER_HOP * 2 # Fund generously

        # create channel between source and dest nodes, no relay node between them
        async with create_channel(swarm3[src], swarm3[dest], funding=channel_funding):
            # Allow time for channel setup
            await asyncio.sleep(2.0)

            for packet in packets:
                random_tag = gen_random_tag() # Unique tag per packet for check
                content_to_send = packet["content"]
                expected_at_dest = [content_to_send]

                start_time = time.monotonic()

                # send message
                send_success = await swarm3[src].api.send_message(
                    swarm3[dest].peer_id,
                    content_to_send,
                    [],
                    random_tag
                )
                if not send_success:
                    logging.error(f"Failed to send packet {packet['index']}")
                    continue # Skip timing if send failed

                min_delay_ms = nodes_config.get(src, {}).get("HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS", 0)
                range_ms = nodes_config.get(src, {}).get("HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS", 0)
                max_delay_ms = min_delay_ms + range_ms
                timeout_value_secs = (max_delay_ms / 1000.0) + 15.0

                # wait for the message to be delivered to dest node
                # and note down the time
                try:
                    await asyncio.wait_for(
                        check_received_packets_with_pop(swarm3[dest], expected_at_dest, tag=random_tag, sort=False),
                        timeout=timeout_value_secs
                    )
                    end_time = time.monotonic()
                    transfer_time = end_time - start_time
                    packet_times[packet["index"]] = transfer_time
                    # logging.debug(f"Packet {packet['index']} transfer time: {transfer_time:.4f} seconds")

                except asyncio.TimeoutError:
                    logging.error(f"Timeout waiting for packet {packet['index']} (tag: {random_tag})")
                except Exception as e:
                    logging.error(f"Error waiting for packet {packet['index']}: {e}")


        
        # plot and safe the transfer values
        transfer_times = list(packet_times.values())
        if not transfer_times:
             pytest.fail("No successful packet transfers recorded.")


        local1_delay = nodes_config.get("local1", {}).get("HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS", 'N/A')
        local1_delay_range = nodes_config.get("local1", {}).get("HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS", 'N/A')
        local2_delay = nodes_config.get("local2", {}).get("HOPR_INTERNAL_MIXER_MINIMUM_DELAY_IN_MS", 'N/A')
        local2_delay_range = nodes_config.get("local2", {}).get("HOPR_INTERNAL_MIXER_DELAY_RANGE_IN_MS", 'N/A')
        config_details = {
            'local1_delay': local1_delay,
            'local2_delay': local2_delay,
            'local1_delay_range': local1_delay_range,
            'local2_delay_range': local2_delay_range
        }

        # create plot for the data and save it
        plot_filename, stats = plot_transfer_time_histogram(transfer_times, config_details)

        if plot_filename and stats:
            logging.info(f"Statistics for configuration - Local1 Delay: {local1_delay}ms (Range: {local1_delay_range}ms), Local2 Delay: {local2_delay}ms (Range: {local2_delay_range}ms)")
            logging.info(f"Average transfer time: {stats['mean']:.4f} seconds")
            logging.info(f"Standard deviation: {stats['std_dev']:.4f} seconds")
        else:
             logging.error("Plotting failed or no data to plot.")


        # Save transfer times to for later view
        csv_filename = os.path.join("logs", f"transfer_times_{timestamp}_l1_{local1_delay}r{local1_delay_range}_l2_{local2_delay}r{local2_delay_range}.csv")

        with open(csv_filename, 'w') as f:
            f.write("packet_index,transfer_time\n")
            if packet_times:
                for idx, time_val in packet_times.items():
                    f.write(f"{idx},{time_val}\n")
                logging.info(f"Transfer times saved to: {csv_filename}")
            else:
                logging.warning("No packet times recorded to save to CSV.")
