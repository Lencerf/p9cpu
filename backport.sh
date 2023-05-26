#!/bin/bash
set -e

mkdir -p p9cpu
for file in $(ls -A); do
    if [ $file != "p9cpu" ] && [ $file != ".git" ] && [ $file != "backport.sh" ]; then
        mv $file p9cpu/
    fi
done
git add p9cpu
git commit -m "tmp commit 1"
git am $1
HASH=$(git rev-parse HEAD)
for file in $(ls -A p9cpu); do 
    mv p9cpu/$file .
done
rm -r p9cpu
git reset --soft HEAD~2
git add .
git commit -m "$(git log -1 --format=%B $HASH)"