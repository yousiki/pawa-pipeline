"""Geometric/clock gates; rendered collision review is still mandatory."""
import math
import unittest
from regional_cinematic_camera import camera_path


class CameraTests(unittest.TestCase):
    def evidence(self, side=4):
        points = [{'position': [(x-(side-1)/2)*18, 74., (z-(side-1)/2)*18]} for x in range(side) for z in range(side)]
        summary = dict(recording_start_world_age_tick=100, recording_stop_world_age_tick=1550,
                       diagnostics={'regional_staging': {'verified': True, 'placements': points}})
        audit = dict(pass_=True, anchor=[0,0], side=side, pitch=18,
                     common_first_world_tick=180, common_last_world_tick=1380)
        audit['pass'] = audit.pop('pass_')
        return summary, audit

    def test_clock_endpoints_and_full_movement_at_both_populations(self):
        for side in (4,6):
            keys, report = camera_path(*self.evidence(side), fps=20)
            self.assertEqual(keys[0]['time_ms'], 0)
            self.assertEqual(report['common_duration_ms'], 60000)
            self.assertGreater(report['path_length_blocks'], side*17)
            self.assertLess(report['camera_y_range'][1], 94)
            self.assertGreater(report['camera_y_range'][0], 81)
            self.assertEqual(keys[1200]['x'], keys[-1]['x'])
            self.assertTrue(all(a['time_ms'] < b['time_ms'] for a,b in zip(keys,keys[1:])))
            self.assertTrue(all(math.isfinite(p[k]) for p in keys for k in ('x','y','z','yaw','pitch')))

    def test_camera_speed_and_yaw_have_no_jumps(self):
        keys, _ = camera_path(*self.evidence(6), fps=20)
        self.assertLess(max(math.dist([a[k] for k in ('x','y','z')],[b[k] for k in ('x','y','z')])*20 for a,b in zip(keys,keys[1:])), 4)
        self.assertLess(max(abs(b['yaw']-a['yaw']) for a,b in zip(keys,keys[1:])), 1)

    def test_recording_overhead_does_not_create_a_frozen_tail(self):
        keys, report = camera_path(*self.evidence(), fps=20, duration_ms=77000)
        self.assertEqual(report['common_duration_ms'], 60000)
        self.assertNotEqual(keys[1200]['x'], keys[1500]['x'])
        self.assertEqual(report['camera_duration_ms'], 77000)

    def test_failed_audit_is_not_presentable(self):
        summary, audit = self.evidence()
        audit['pass'] = False
        with self.assertRaises(ValueError):
            camera_path(summary, audit, 20)


if __name__ == '__main__':
    unittest.main()
