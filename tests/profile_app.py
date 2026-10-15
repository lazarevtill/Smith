#!/usr/bin/env python3
"""Default automatic local workspace HTTP acceptance."""
import argparse,sys
from pathlib import Path
from local_workspace_app import main
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--binary',default=str(Path(__file__).resolve().parents[1]/'target/debug/smith-next'));args=parser.parse_args();sys.argv=[sys.argv[0],args.binary];main()
