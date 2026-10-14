"""The portable text of a build command read as a table (feverlink3_portable.lua), with real texts of the native module when the local
research files exist (docs/research/spike-dumps-20261013/s0s1) and a hand-written depot-shaped text always."""
import os
import struct
import unittest

import lupa.lua52 as lupa

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "..", "..", "mod", "feverlink3_1", "content", "feverlink3_portable.lua")
REAL = os.path.join(HERE, "..", "..", "docs", "research", "spike-dumps-20261013", "s0s1", "guest", "native_portable.log")


BS = chr(92)


def f32(x):
    return "%08x" % struct.unpack("<I", struct.pack("<f", x))[0]


def load():
    L = lupa.LuaRuntime(unpack_returned_tuples=True)
    return L, L.execute(open(SRC, encoding="utf-8").read())


DEPOT = "\n".join([
    "=== portable v1 layout=40420 event=input key=none site=none tool=none seq=0 t_ms=1 wall_ms=0 payload=0x1 ===",
    "proposal.addedNodes.#\tn:2",
    "proposal.addedNodes[0].entity\tent:new#1",
    "proposal.addedNodes[0].position\tv3:%s,%s,%s" % (f32(1388.5), f32(304.25), f32(-5.375)),
    "proposal.addedNodes[1].entity\tent:new#2",
    "proposal.addedNodes[1].position\tv3:%s,%s,%s" % (f32(1.0), f32(2.0), f32(3.0)),
    "proposal.addedSegments.#\tn:1",
    "proposal.addedSegments[0].entity\tent:new#3",
    "proposal.addedSegments[0].node0\tent:old#39106@%s,%s,%s" % (f32(1369.5), f32(300.0), f32(5.0)),
    "proposal.addedSegments[0].node1\tent:new#1",
    "proposal.addedSegments[0].typeIndex\ti32:ffffffff",
    "proposal.addedSegments[0].laneConfigs.bytes\tn:96",
    "proposal.addedSegments[0].roadTemplate\tres:\"infrastructure/street/constructions/entrance_new.street_template\"",
    "proposal.addedSegments[0].roadStyle	res:"+'"'+"a "+BS+'"'+"quoted"+BS+'"'+" "+BS+"x41 "+BS+BS+" name"+'"',
    "proposal.addedSegments[0].playerOwned\tent:old#34135",
    "proposal.removedNodes.#\tn:0",
    "proposal.nodeConfigsToAdd.#\tn:1",
    "proposal.nodeConfigsToAdd[0].entity\tent:old#39106",
    "proposal.hash90\thash:size=0,load=3f800000",
    "proposal.frozenNodes.#\tn:1",
    "proposal.frozenNodes[0]\tent:old#77",
    "toRemove.#\tn:2",
    "toRemove[0]\tent:old#39259",
    "toRemove[1]\tent:old#39481",
    "toAdd.#\tn:1",
    "toAdd[0].fileName\tres:\"depots/road/road_depot/road_depot.con\"",
    "toAdd[0].~0x40\traw:z1 P",
    "toAdd[0].transf\tm4:" + ",".join(f32(x) for x in [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 10.5, 20.5, 5.5, 1]),
    "toAdd[0].name\tstr:\"Road Depot\"",
    "toAdd[0].playerEntity\tent:old#34135",
    "context.gatherBuildings\tu8:01",
    "context.cleanupStreetGraph\tu8:00",
    "ignoreErrors\tu8:00",
    "playerInitiated\tu8:01",
    "kind\tu8:34",
    "=== end portable #0 lines=1 ===",
])


class PortableTest(unittest.TestCase):
    def setUp(self):
        self.L, self.M = load()

    def parse(self, text):
        t, info = self.M.parse(text)
        self.assertIsNotNone(t, info)
        return t

    def test_floats_come_from_their_bits(self):
        for x in (0.0, 1.0, -2.5, 1388.276, 5.374, 1e-40, 3.4e38):
            bits = int(f32(x), 16)
            self.assertAlmostEqual(self.M.float(bits), struct.unpack("<f", struct.pack("<f", x))[0], places=30 if abs(x) < 1e-30 else 4)
        self.assertEqual(self.M.float(0x7F800000), float("inf"))

    def test_a_depot_shaped_text_becomes_a_tree(self):
        t = self.parse(DEPOT)
        p = t.proposal
        self.assertEqual((p.addedNodes.n, p.addedSegments.n, p.removedNodes.n, p.nodeConfigsToAdd.n), (2, 1, 0, 1))
        self.assertEqual(p.addedNodes[1].entity.new, 1)
        self.assertAlmostEqual(p.addedNodes[1].position.x, 1388.5)
        self.assertAlmostEqual(p.addedNodes[1].position.z, -5.375)
        seg = p.addedSegments[1]
        self.assertEqual((seg.node0.old, seg.node1.new, seg.entity.new), (39106, 1, 3))
        self.assertAlmostEqual(seg.node0.pos.x, 1369.5)
        self.assertEqual(seg.typeIndex, -1)
        self.assertEqual(seg.laneConfigs.bytes, 96)
        self.assertEqual(seg.roadTemplate, "infrastructure/street/constructions/entrance_new.street_template")
        self.assertEqual(seg.roadStyle, 'a "quoted" A ' + BS + ' name')
        self.assertEqual(seg.playerOwned.old, 34135)
        self.assertEqual((t.toRemove.n, t.toRemove[1].old, t.toRemove[2].old), (2, 39259, 39481))
        self.assertEqual(t.toAdd[1].fileName, "depots/road/road_depot/road_depot.con")
        self.assertEqual(t.toAdd[1].name, "Road Depot")
        self.assertAlmostEqual(t.toAdd[1].transf[13], 10.5)
        self.assertEqual((t.context.gatherBuildings, t.context.cleanupStreetGraph, t.ignoreErrors, t.playerInitiated, t.kind), (1, 0, 0, 1, 52))
        self.assertEqual(p.frozenNodes[1].old, 77)

    def test_what_carries_no_input_is_skipped(self):
        t = self.parse(DEPOT)
        self.assertIsNone(t.proposal.hash90)
        self.assertIsNone(t.toAdd[1]["~0x40"])

    def test_counts_are_those_of_the_build_record(self):
        c = self.M.counts(self.parse(DEPOT))
        self.assertEqual(self.M.countsText(c), "cons=1 rem=2 nodes=2 segs=1 rmNodes=0 rmSegs=0 objs=0 rmObjs=0 ncfg=1 rmNcfg=0 frozen=1")

    def test_a_text_that_cannot_be_read_is_refused_whole(self):
        for bad in ("", "no tab here", "a\tu32:zz"):
            t, why = self.M.parse(bad)
            self.assertIsNone(t, bad)
            self.assertTrue(why)

    @unittest.skipUnless(os.path.exists(REAL), "local research file")
    def test_the_real_held_road_of_the_spike_run(self):
        text = open(REAL, encoding="utf-8", errors="replace").read()
        part = [p for p in text.split("=== portable v1")[1:] if "event=held" in p.split("\n")[0]][0]
        t = self.parse("=== portable v1" + part)
        self.assertEqual(self.M.countsText(self.M.counts(t)), "cons=0 rem=0 nodes=2 segs=1 rmNodes=0 rmSegs=0 objs=0 rmObjs=0 ncfg=2 rmNcfg=0 frozen=0")
        seg = t.proposal.addedSegments[1]
        self.assertEqual(seg.roadTemplate, "infrastructure/street/town/town_new_small.street_template")
        self.assertEqual((seg.node0.new, seg.node1.new, seg.playerOwned.old), (1, 2, 34135))
        self.assertEqual((t.context.checkTerrainAlignment, t.context.extendProposalRedoPillars, t.playerInitiated), (1, 1, 1))

    @unittest.skipUnless(os.path.exists(REAL), "local research file")
    def test_every_real_text_of_the_spike_run_reads(self):
        text = open(REAL, encoding="utf-8", errors="replace").read()
        parts = text.split("=== portable v1")[1:]
        for part in parts:
            if " REFUSED " in part.split(chr(10))[0]:
                t, why = self.M.parse("=== portable v1" + part)
                self.assertIsNone(t, "a refused text is not data")
                continue
            t, info = self.M.parse("=== portable v1" + part)
            self.assertIsNotNone(t, info)
            self.assertEqual(t.kind, 52)

    def refs(self, text):
        out = []
        for r in self.M.oldRefs(text).values():
            pos = r.pos and (round(r.pos.x, 2), round(r.pos.y, 2), round(r.pos.z, 2))
            out.append((str(r.path), int(r.id), str(r.kind), pos))
        return out

    def test_the_existing_entities_a_text_names_with_their_kind(self):
        text = DEPOT + "\nproposal.old2newSegments.keys\tkeys:000098c9=[fffffffb;fffffffc]\nproposal.new2oldNodes.keys\tkeys:fffffffa=[00009a00]\n"
        self.assertEqual(self.refs(text), [
            ("proposal.addedSegments[0].node0", 39106, "node", (1369.5, 300.0, 5.0)),
            ("toRemove[0]", 39259, "construction", None),
            ("toRemove[1]", 39481, "construction", None),
            ("proposal.old2newSegments.keys", 0x98c9, "edge", None),
            ("proposal.new2oldNodes.keys", 0x9a00, "node", None),
        ], "playerOwned, playerEntity and frozenNodes are not checked; node 39106 is named twice and listed once")

    CRASH = os.path.join(HERE, "..", "..", "docs", "research", "rail-crash-run-20261014", "host", "native_inject_3.txt")

    @unittest.skipUnless(os.path.exists(CRASH), "local research file")
    def test_the_track_of_the_rail_crash_names_one_existing_node(self):
        with open(self.CRASH, encoding="utf-8") as f:
            text = f.read()
        refs = self.refs(text)
        self.assertEqual([(p, i, k) for p, i, k, _ in refs], [("proposal.addedSegments[0].node0", 50395, "node")])
        self.assertIsNotNone(refs[0][3], "with the position the receiver compares")


if __name__ == "__main__":
    unittest.main()
