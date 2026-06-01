<?php

declare(strict_types=1);

namespace App\PhpDoc;

/**
 * CASE: Usage sites for PHPDoc virtual members from SupportedTags.
 */
function exerciseVirtualMembers(SupportedTags $subject): void
{
    $label = $subject->label;
    $name = $subject->owner->getName();
    $user = $subject->findById(1);
    $subject->dirty = false;

    renderUser($user);
    echo $label;
    echo $name;
}
