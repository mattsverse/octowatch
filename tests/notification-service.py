#!/usr/bin/env python3
"""Fake Linux notification service for the opt-in native contract test.

Run only on an isolated session bus (dbus-run-session), never on a desktop bus.
Requires python3-dbus and python3-gi; writes accepted requests to stdout.
"""
import json

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

INTERFACE = "org.freedesktop.Notifications"


class Notifications(dbus.service.Object):
    def __init__(self, bus):
        super().__init__(bus, "/org/freedesktop/Notifications")
        self.next_id = 1

    @dbus.service.method(INTERFACE, in_signature="", out_signature="as")
    def GetCapabilities(self):
        return ["actions", "body"]

    @dbus.service.method(INTERFACE, in_signature="", out_signature="ssss")
    def GetServerInformation(self):
        return ("Octowatcher test fixture", "Octowatcher", "1", "1.2")

    @dbus.service.method(INTERFACE, in_signature="susssasa{sv}i", out_signature="u")
    def Notify(self, app, replaces, icon, summary, body, actions, hints, timeout):
        if body == "denied":
            raise dbus.exceptions.DBusException(
                "Notifications denied", name="org.freedesktop.DBus.Error.AccessDenied"
            )
        if body == "failure":
            raise dbus.exceptions.DBusException(
                "Injected delivery failure", name="org.freedesktop.DBus.Error.Failed"
            )
        ids = list(actions[::2])
        if "default" not in ids:
            raise dbus.exceptions.DBusException(
                "Missing advertised body-click action",
                name="org.freedesktop.DBus.Error.InvalidArgs",
            )
        notification_id = self.next_id
        self.next_id += 1
        print(json.dumps({"id": notification_id, "body": str(body), "actions": ids}), flush=True)
        if body.startswith("click:"):
            action = body.removeprefix("click:")
            if action not in ids:
                raise dbus.exceptions.DBusException(
                    "Requested test action not advertised",
                    name="org.freedesktop.DBus.Error.InvalidArgs",
                )
            GLib.timeout_add(200, self.emit_action, notification_id, action)
        elif body == "dismiss":
            GLib.timeout_add(200, self.emit_closed, notification_id)
        return dbus.UInt32(notification_id)

    def emit_action(self, notification_id, action):
        self.ActionInvoked(notification_id, action)
        return False

    def emit_closed(self, notification_id):
        self.NotificationClosed(notification_id, 2)
        return False

    @dbus.service.method(INTERFACE, in_signature="u", out_signature="")
    def CloseNotification(self, notification_id):
        print(json.dumps({"closed": int(notification_id)}), flush=True)
        self.NotificationClosed(notification_id, 3)

    @dbus.service.signal(INTERFACE, signature="us")
    def ActionInvoked(self, notification_id, action):
        pass

    @dbus.service.signal(INTERFACE, signature="uu")
    def NotificationClosed(self, notification_id, reason):
        pass


DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName(INTERFACE, bus)
service = Notifications(bus)
print("ready", flush=True)
GLib.MainLoop().run()
