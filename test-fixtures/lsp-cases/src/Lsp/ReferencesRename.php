<?php

declare(strict_types=1);

namespace App\Lsp;

use App\Model\User;

// CASE: Primary rename target class.
class RenameTarget
{
    // CASE: Property rename/references target.
    public string $status = 'new';

    // CASE: Class constant rename/references target.
    public const STATE_ACTIVE = 'active';

    // CASE: Method rename/references target.
    public function touch(User $user): void
    {
        $this->status = self::STATE_ACTIVE;
        $user->getName();
    }
}

// CASE: References for class in parameter and constructor call.
function useRenameTarget(RenameTarget $target): void
{
    $local = new RenameTarget();

    // CASE: Method references.
    $target->touch(new User('A'));
    $local->touch(new User('B'));

    // CASE: Property references.
    $target->status = RenameTarget::STATE_ACTIVE;
    echo $local->status;
}

