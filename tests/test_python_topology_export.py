from __future__ import annotations

import struct
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PYTHON_FILES = ROOT / "python-files"
if str(PYTHON_FILES) not in sys.path:
    sys.path.insert(0, str(PYTHON_FILES))

try:
    import sedsprintf_rs as seds
except Exception as exc:  # pragma: no cover - environment dependent
    seds = None
    IMPORT_ERROR = exc
else:
    IMPORT_ERROR = None


@unittest.skipIf(
    seds is None or not hasattr(seds, "endpoint_exists") or not hasattr(seds, "make_packet"),
    f"sedsprintf_rs binding unavailable: {IMPORT_ERROR}",
)
class PythonTopologyExportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not seds.endpoint_exists(9102):
            seds.register_endpoint(9102, "PY_TOPOLOGY_RADIO_9102")
        cls.radio = seds.endpoint_info_by_name("PY_TOPOLOGY_RADIO_9102")["id"]
        cls.discovery_endpoint = int(seds.DataEndpoint.Discovery)
        cls.discovery_announce = int(seds.DataType.DiscoveryAnnounce)

    def make_discovery_announce(self, sender: str):
        payload = struct.pack("<I", self.radio)
        return seds.make_packet(
            self.discovery_announce,
            sender,
            [self.discovery_endpoint],
            0,
            payload,
        )

    def assert_topology_shape(self, topology: dict, local_sender: str) -> None:
        self.assertIn("routers", topology)
        self.assertIn("routes", topology)
        self.assertIn("advertised_endpoints", topology)
        self.assertIn("advertised_timesync_sources", topology)
        self.assertIsInstance(topology["routers"], list)
        self.assertIsInstance(topology["routes"], list)
        self.assertIsInstance(topology["advertised_endpoints"], list)
        self.assertIsInstance(topology["advertised_timesync_sources"], list)
        self.assertEqual(len(topology["routes"]), 1)

        route = topology["routes"][0]
        self.assertIn("announcers", route)
        self.assertIsInstance(route["side_id"], int)
        self.assertIsInstance(route["side_name"], str)
        self.assertIsInstance(route["reachable_endpoints"], list)
        self.assertIsInstance(route["reachable_timesync_sources"], list)
        self.assertIsInstance(route["last_seen_ms"], int)
        self.assertIsInstance(route["age_ms"], int)
        self.assertEqual(len(route["announcers"]), 1)
        announcer = route["announcers"][0]
        self.assertEqual(announcer["sender_id"], "REMOTE_NODE")
        self.assertIn("routers", announcer)
        self.assertIsInstance(announcer["last_seen_ms"], int)
        self.assertIsInstance(announcer["age_ms"], int)
        self.assertGreaterEqual(len(announcer["routers"]), 1)

        remote_board = next(
            board for board in announcer["routers"] if board["sender_id"] == "REMOTE_NODE"
        )
        self.assertEqual(remote_board["reachable_endpoints"], [self.radio])
        self.assertIsInstance(remote_board["reachable_timesync_sources"], list)
        self.assertIsInstance(remote_board["connections"], list)
        self.assertIn(local_sender, remote_board["connections"])
        self.assertIn("reachable_timesync_sources", remote_board)

        local_board = next(
            board for board in topology["routers"] if board["sender_id"] == local_sender
        )
        self.assertIn("connections", local_board)
        self.assertIn("REMOTE_NODE", local_board["connections"])

    def assert_runtime_stats_shape(self, stats: dict, expected_side_name: str) -> None:
        self.assertIn("sides", stats)
        self.assertIn("route_modes", stats)
        self.assertIn("route_overrides", stats)
        self.assertIn("typed_route_overrides", stats)
        self.assertIn("route_weights", stats)
        self.assertIn("route_priorities", stats)
        self.assertIn("queues", stats)
        self.assertIn("reliable", stats)
        self.assertIn("discovery", stats)
        self.assertIn("total_handler_failures", stats)
        self.assertIn("total_handler_retries", stats)
        self.assertIsInstance(stats["total_handler_failures"], int)
        self.assertIsInstance(stats["total_handler_retries"], int)
        self.assertGreaterEqual(len(stats["sides"]), 1)

        side = next(item for item in stats["sides"] if item["side_name"] == expected_side_name)
        self.assertIsInstance(side["side_id"], int)
        self.assertIsInstance(side["reliable_enabled"], bool)
        self.assertIsInstance(side["link_local_enabled"], bool)
        self.assertIsInstance(side["ingress_enabled"], bool)
        self.assertIsInstance(side["egress_enabled"], bool)
        self.assertIn("tx_packets", side)
        self.assertIn("rx_packets", side)
        self.assertIn("relayed_tx_packets", side)
        self.assertIn("tx_retries", side)
        self.assertIn("adaptive", side)
        self.assertIn("data_types", side)
        self.assertIsInstance(side["tx_packets"], int)
        self.assertIsInstance(side["tx_bytes"], int)
        self.assertIsInstance(side["rx_packets"], int)
        self.assertIsInstance(side["rx_bytes"], int)
        self.assertIsInstance(side["data_types"], list)
        self.assertIn("estimated_capacity_bps", side["adaptive"])
        self.assertIn("peak_usage_bps", side["adaptive"])
        self.assertIn("effective_weight", side["adaptive"])
        self.assertIsInstance(side["adaptive"]["auto_balancing_enabled"], bool)
        self.assertIsInstance(side["adaptive"]["estimated_capacity_bps"], int)
        self.assertIsInstance(side["adaptive"]["peak_capacity_bps"], int)
        self.assertIsInstance(side["adaptive"]["current_usage_bps"], int)
        self.assertIsInstance(side["adaptive"]["peak_usage_bps"], int)
        self.assertIsInstance(side["adaptive"]["available_headroom_bps"], int)
        self.assertIsInstance(side["adaptive"]["effective_weight"], int)
        self.assertIsInstance(side["adaptive"]["last_observed_ms"], int)
        self.assertIsInstance(side["adaptive"]["sample_count"], int)

        if side["data_types"]:
            item = side["data_types"][0]
            self.assertIsInstance(item["data_type"], int)
            self.assertIsInstance(item["tx_packets"], int)
            self.assertIsInstance(item["tx_bytes"], int)
            self.assertIsInstance(item["rx_packets"], int)
            self.assertIsInstance(item["rx_bytes"], int)
            self.assertIsInstance(item["relayed_tx_packets"], int)
            self.assertIsInstance(item["relayed_rx_packets"], int)
            self.assertIsInstance(item["tx_retries"], int)
            self.assertIsInstance(item["handler_failures"], int)

        route_mode = stats["route_modes"][0] if stats["route_modes"] else None
        if route_mode is not None:
            self.assertIn(route_mode["selection_mode"], (None, "Fanout", "Weighted", "Failover"))
            self.assertIsInstance(route_mode["cursor"], int)

        discovery = stats["discovery"]
        self.assertIn("route_count", discovery)
        self.assertIn("announcer_count", discovery)
        self.assertIsInstance(discovery["route_count"], int)
        self.assertIsInstance(discovery["announcer_count"], int)

        queues = stats["queues"]
        self.assertIsInstance(queues["rx_len"], int)
        self.assertIsInstance(queues["tx_len"], int)
        self.assertIsInstance(queues["shared_queue_bytes_used"], int)

        reliable = stats["reliable"]
        self.assertIsInstance(reliable["reliable_return_route_count"], int)
        self.assertIsInstance(reliable["end_to_end_pending_count"], int)

    def test_router_export_topology_exposes_graph_shape(self) -> None:
        router = seds.Router()
        side_id = router.add_side_packet("UPLINK", lambda pkt: None)
        router.rx_packet_from_side(side_id, self.make_discovery_announce("REMOTE_NODE"))

        self.assert_topology_shape(router.export_topology(), "TEST_PLATFORM")

    def test_relay_export_topology_exposes_graph_shape(self) -> None:
        relay = seds.Relay()
        side_id = relay.add_side_packet("UPLINK", lambda pkt: None)
        relay.rx_packet_from_side(side_id, self.make_discovery_announce("REMOTE_NODE"))

        self.assert_topology_shape(relay.export_topology(), "RELAY")

    def test_router_export_runtime_stats_exposes_diagnostics_shape(self) -> None:
        router = seds.Router()
        side_id = router.add_side_packet("UPLINK", lambda pkt: None)
        router.rx_packet_from_side(side_id, self.make_discovery_announce("REMOTE_NODE"))

        stats = router.export_runtime_stats()
        self.assert_runtime_stats_shape(stats, "UPLINK")
        self.assertGreaterEqual(stats["discovery"]["route_count"], 1)
        side = next(item for item in stats["sides"] if item["side_name"] == "UPLINK")
        self.assertGreaterEqual(side["rx_packets"], 1)

    def test_relay_export_runtime_stats_exposes_diagnostics_shape(self) -> None:
        relay = seds.Relay()
        side_id = relay.add_side_packet("UPLINK", lambda pkt: None)
        relay.rx_packet_from_side(side_id, self.make_discovery_announce("REMOTE_NODE"))

        stats = relay.export_runtime_stats()
        self.assert_runtime_stats_shape(stats, "UPLINK")
        side = next(item for item in stats["sides"] if item["side_name"] == "UPLINK")
        self.assertGreaterEqual(side["rx_packets"], 1)


if __name__ == "__main__":
    unittest.main()
