#!/bin/sh
#
# PROVIDE: priosun
# REQUIRE: NETWORKING pf
# KEYWORD: shutdown

. /etc/rc.subr

name="priosun"
rcvar="priosun_enable"

load_rc_config $name

: ${priosun_enable:="NO"}
: ${priosun_socket_dir:="/var/run"}
: ${priosun_pid_file:="/var/run/priosun.pid"}

command="/usr/local/bin/priosund"
command_args="--no-daemon"

run_rc_command "$1"
