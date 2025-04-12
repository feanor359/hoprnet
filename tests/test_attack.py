import asyncio
import random
import re
import string
import logging
import time
from contextlib import AsyncExitStack, asynccontextmanager

import pytest

from sdk.python.api import HoprdAPI
from sdk.python.api.channelstatus import ChannelStatus
from sdk.python.localcluster.constants import (
    OPEN_CHANNEL_FUNDING_VALUE_HOPR,
    TICKET_PRICE_PER_HOP,
)
from sdk.python.localcluster.node import Node

from .conftest import attacking_nodes, first_two_from
from .utils import (
    AGGREGATED_TICKET_PRICE,
    MULTIHOP_MESSAGE_SEND_TIMEOUT,
    PARAMETERIZED_SAMPLE_SIZE,
    RESERVED_TAG_UPPER_BOUND,
    TICKET_AGGREGATION_THRESHOLD,
    check_all_tickets_redeemed,
    check_native_balance_below,
    check_received_packets_with_peek,
    check_rejected_tickets_value,
    check_safe_balance,
    check_unredeemed_tickets_value,
    create_channel,
    gen_random_tag,
    send_and_receive_packets_with_pop,
    shuffled,
)

# used by nodes to get unique port assignments
PORT_BASE = 19000

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
        print(f"Async operation took {duration:.4f} seconds")

        if src == "local1":
            assert duration < 5, "The message was not delayed as expected"
            assert duration > 6, "The message was delayed too long"

        elif src == "local2":
            assert duration > 1, "The message was delayed too long"
